package io.ironmesh.android.ui

import android.app.Application
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.DocumentsContract
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.ProcessLifecycleOwner
import androidx.lifecycle.viewModelScope
import io.ironmesh.android.BuildConfig
import io.ironmesh.android.api.StoreIndexEntry
import io.ironmesh.android.api.StoreIndexResponse
import io.ironmesh.android.api.StoreIndexSortOrder
import io.ironmesh.android.data.ConnectionRouteSnapshot
import io.ironmesh.android.data.AndroidDiagnosticLog as Log
import io.ironmesh.android.data.buildAndroidDiagnosticLogExport
import io.ironmesh.android.data.DeviceAuthState
import io.ironmesh.android.data.EnrollmentAccessVerification
import io.ironmesh.android.data.EmbeddedWebUiSession
import io.ironmesh.android.data.EmbeddedWebUiSessionRegistry
import io.ironmesh.android.data.DeviceIdentityStorageException
import io.ironmesh.android.data.FolderSyncConfig
import io.ironmesh.android.data.AppConnectionStatus
import io.ironmesh.android.data.FolderSyncNetworkPolicy
import io.ironmesh.android.data.FolderSyncModificationRecord
import io.ironmesh.android.data.GlobalFolderSyncStatus
import io.ironmesh.android.data.FolderSyncServiceStatus
import io.ironmesh.android.data.mergeGlobalFolderSyncStatus
import io.ironmesh.android.ui.screens.ThumbnailBitmapCache
import io.ironmesh.android.data.IronmeshPreferences
import io.ironmesh.android.data.IronmeshRepository
import io.ironmesh.android.data.RustPreferencesBridge
import io.ironmesh.android.data.TitleLatencyMonitorSettings
import io.ironmesh.android.data.TitleLatencyProbeStatus
import io.ironmesh.android.data.parseAllowedWifiSsidsInput
import io.ironmesh.android.ui.theme.DEFAULT_IRONMESH_ACCENT_COLOR_HEX
import io.ironmesh.android.ui.theme.normalizeIronmeshAccentColorHex
import io.ironmesh.android.work.FolderSyncScheduler
import io.ironmesh.android.work.FolderSyncNetworkGate
import io.ironmesh.android.work.FolderSyncExecutionCoordinator
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.filterNotNull
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import java.util.UUID

enum class GallerySortOption {
    CREATION_TIME,
    NAME,
}

enum class GalleryViewMode {
    FLATTENED_ALL_IMAGES,
    CURRENT_DIRECTORY,
}

enum class MainSection {
    HOME,
    CONNECTIVITY,
    REQUEST_TIMINGS,
    SYNC,
    LIBRARY,
    GALLERY_MAP,
    SETTINGS,
}

enum class FolderSyncActivityFilter {
    ALL,
    UPLOADS,
    DOWNLOADS,
    DELETES,
}

private const val GALLERY_ROOT_DOCUMENT_ID = "dir:"
private const val GALLERY_ROOT_PATH = "/"
private const val GALLERY_PAGE_SIZE = 32
private const val GALLERY_PAGE_PRELOAD_RADIUS = 1
private const val GALLERY_PAGE_KEEP_RADIUS = 2
private const val GALLERY_FLATTENED_DEPTH = 64
private const val FOLDER_SYNC_HISTORY_PAGE_SIZE = 20
private const val FOLDER_SYNC_HISTORY_REFRESH_MS = 5_000L
private const val CONNECTION_ROUTE_SNAPSHOT_POLL_MS = 1_000L
private const val TITLE_LATENCY_STATUS_POLL_MS = 1_000L
private const val ENROLLMENT_VERIFICATION_POLL_MS = 5_000L
private const val ENROLLMENT_LOG_TAG = "EnrollmentDiagnostics"

data class FolderSyncHistoryState(
    val expanded: Boolean = false,
    val records: List<FolderSyncModificationRecord> = emptyList(),
    val nextBeforeId: Long? = null,
    val filter: FolderSyncActivityFilter = FolderSyncActivityFilter.ALL,
    val loading: Boolean = false,
    val error: String? = null,
    val lastLoadedElapsedRealtimeMs: Long = 0L,
)

data class GalleryImageItem(
    val documentUri: Uri,
    val displayName: String,
    val remotePath: String,
    val mimeType: String,
    val createdAtUnixMs: Long? = null,
    val width: Int? = null,
    val height: Int? = null,
    val thumbnailStatus: String? = null,
)

data class GalleryDirectoryItem(
    val documentId: String,
    val displayName: String,
    val pathLabel: String,
)

data class GalleryBreadcrumbItem(
    val documentId: String,
    val label: String,
    val pathLabel: String,
)

data class GalleryCollectionState(
    val totalItemCount: Int,
    val pageSize: Int,
    val pageCount: Int,
)

enum class GalleryPageStatus {
    LOADING,
    READY,
    ERROR,
}

data class GalleryPageState(
    val status: GalleryPageStatus,
    val items: List<GalleryImageItem> = emptyList(),
    val error: String? = null,
)

enum class GalleryLoadErrorKind {
    TIMEOUT,
    REQUEST_FAILED,
}

data class GalleryLoadError(
    val kind: GalleryLoadErrorKind,
    val technicalDetail: String? = null,
)

data class MainUiState(
    val persistedStateLoaded: Boolean = false,
    val deviceAuthState: DeviceAuthState = DeviceAuthState(),
    val bootstrapInput: String = "",
    val deviceLabelInput: String = "",
    val enrollmentDiagnostics: List<EnrollmentDiagnosticStep> = emptyList(),
    val key: String = "demo-key",
    val payload: String = "hello from android",
    val status: String = "Ready",
    val objectBody: String = "",
    val syncProfiles: List<FolderSyncConfig> = emptyList(),
    val folderSyncStatus: FolderSyncServiceStatus = FolderSyncServiceStatus(),
    val globalFolderSyncStatus: GlobalFolderSyncStatus = GlobalFolderSyncStatus(),
    val appConnectionStatus: AppConnectionStatus = AppConnectionStatus(),
    val titleLatencyMonitorSettings: TitleLatencyMonitorSettings = TitleLatencyMonitorSettings(),
    val titleLatencyStatus: TitleLatencyProbeStatus = TitleLatencyProbeStatus(),
    val folderSyncHistory: Map<String, FolderSyncHistoryState> = emptyMap(),
    val newSyncLabel: String = "",
    val newSyncPrefix: String = "",
    val newSyncLocalFolder: String = "",
    val newSyncLocalFolderTreeUri: String? = null,
    val newSyncAllowWifi: Boolean = true,
    val newSyncAllowCellular: Boolean = true,
    val newSyncAllowOtherConnections: Boolean = true,
    val newSyncAllowRoaming: Boolean = false,
    val newSyncAllowedWifiSsids: String = "",
    val selectedSection: MainSection = MainSection.HOME,
    val connectionRoutes: ConnectionRouteSnapshot? = null,
    val connectionRoutesLoading: Boolean = false,
    val connectionRoutesError: String? = null,
    val connectionRoutesLastLoadedUnixMs: Long = 0L,
    val timingMeasurementResetting: Boolean = false,
    val timingStoreIndexTestRunning: Boolean = false,
    val timingMeasurementStartedUnixMs: Long? = null,
    val webUiSession: EmbeddedWebUiSession? = null,
    val galleryMode: GalleryViewMode = GalleryViewMode.FLATTENED_ALL_IMAGES,
    val galleryCollection: GalleryCollectionState? = null,
    val galleryPages: Map<Int, GalleryPageState> = emptyMap(),
    val galleryDirectories: List<GalleryDirectoryItem> = emptyList(),
    val galleryBreadcrumbs: List<GalleryBreadcrumbItem> = emptyList(),
    val galleryCurrentDirectoryDocumentId: String = GALLERY_ROOT_DOCUMENT_ID,
    val galleryCurrentDirectoryPath: String = GALLERY_ROOT_PATH,
    val gallerySort: GallerySortOption = GallerySortOption.CREATION_TIME,
    val themeAccentColorHex: String = DEFAULT_IRONMESH_ACCENT_COLOR_HEX,
    val galleryLoading: Boolean = false,
    val galleryError: GalleryLoadError? = null,
    val loading: Boolean = false,
)

private data class TimingDiagnosticsCredentials(
    val connectionInput: String,
    val serverCaPem: String?,
    val clientIdentityJson: String,
)

private data class PersistedMainUiState(
    val syncProfiles: List<FolderSyncConfig>,
    val deviceAuthResult: Result<DeviceAuthState>,
    val galleryViewMode: GalleryViewMode,
    val titleLatencyMonitorSettings: TitleLatencyMonitorSettings,
    val themeAccentColorHex: String,
)

class MainViewModel(
    application: Application,
) : AndroidViewModel(application) {
    private companion object {
        const val WEB_UI_BACKGROUND_GRACE_PERIOD_MILLIS = 10_000L
    }

    private val repository = IronmeshRepository()
    private var galleryRequestVersion = 0
    private var pinnedGalleryItemIndex: Int? = null
    private var folderSyncStatusMonitorJob: Job? = null
    private var appConnectionStatusMonitorJob: Job? = null
    private var connectionRoutesMonitorJob: Job? = null
    private var titleLatencyConfigurationJob: Job? = null
    private var titleLatencyStatusMonitorJob: Job? = null
    private var enrollmentVerificationMonitorJob: Job? = null
    private val uiObservationGate = UiObservationGate()
    private val mainHandler = Handler(Looper.getMainLooper())
    private val preferenceWriteMutex = Mutex()

    var uiState = androidx.compose.runtime.mutableStateOf(MainUiState())
        private set

    private val webUiSessionBackgroundGrace = WebUiSessionBackgroundGrace(
        scheduler = HandlerWebUiSessionStopScheduler(mainHandler),
        gracePeriodMillis = WEB_UI_BACKGROUND_GRACE_PERIOD_MILLIS,
        onGraceExpired = ::stopWebUiAfterBackgroundGrace,
    )

    @Volatile
    private var processLifecycleObserverActive = true
    private var processLifecycleObserverRegistered = false
    private val processLifecycleObserver = LifecycleEventObserver { _, event ->
        when (event) {
            Lifecycle.Event.ON_START -> handleProcessStarted()
            Lifecycle.Event.ON_STOP -> handleProcessStopped()

            else -> Unit
        }
    }

    init {
        registerProcessLifecycleObserver()
        loadPersistedState()
    }

    override fun onCleared() {
        processLifecycleObserverActive = false
        unregisterProcessLifecycleObserver()
        webUiSessionBackgroundGrace.cancelScheduledStop()
        EmbeddedWebUiSessionRegistry.clear()
        stopUiObservation()
        stopEnrollmentVerificationMonitor()
        titleLatencyConfigurationJob?.cancel()
        titleLatencyConfigurationJob = null
        repository.stopWebUi()
        super.onCleared()
    }

    private fun registerProcessLifecycleObserver() {
        runOnMainThread {
            if (processLifecycleObserverActive && !processLifecycleObserverRegistered) {
                ProcessLifecycleOwner.get().lifecycle.addObserver(processLifecycleObserver)
                processLifecycleObserverRegistered = true
            }
        }
    }

    private fun unregisterProcessLifecycleObserver() {
        runOnMainThread {
            if (processLifecycleObserverRegistered) {
                ProcessLifecycleOwner.get().lifecycle.removeObserver(processLifecycleObserver)
                processLifecycleObserverRegistered = false
            }
        }
    }

    private fun runOnMainThread(action: () -> Unit) {
        if (Looper.myLooper() == Looper.getMainLooper()) {
            action()
        } else {
            mainHandler.post(action)
        }
    }

    private fun handleProcessStarted() {
        webUiSessionBackgroundGrace.cancelScheduledStop()
        if (uiObservationGate.enterForeground() == UiObservationTransition.START) {
            startUiObservationJobs()
        }
    }

    private fun handleProcessStopped() {
        if (uiObservationGate.leaveForeground() == UiObservationTransition.STOP) {
            stopUiObservationJobs()
        }
        viewModelScope.launch(Dispatchers.IO) {
            runCatching { RustPreferencesBridge.flushAppConnectionStatus() }
                .onFailure { error ->
                    Log.w("MainViewModel", "Connection status background flush failed", error)
                }
        }
        if (uiState.value.webUiSession != null) {
            webUiSessionBackgroundGrace.scheduleStop()
        }
    }

    private fun startUiObservationJobs() {
        notifyManagedClientForegrounded()
        startAppConnectionStatusMonitor()
        startFolderSyncStatusMonitor()
        if (uiState.value.titleLatencyMonitorSettings.enabled) {
            configureTitleLatencyMonitor()
        }
        if (uiState.value.selectedSection.isConnectionDiagnosticsSection()) {
            startConnectionRoutesMonitor()
        }
    }

    private fun loadPersistedState() {
        viewModelScope.launch {
            val persistedResult = withContext(Dispatchers.IO) {
                runCatching {
                    PersistedMainUiState(
                        syncProfiles = IronmeshPreferences.getFolderSyncConfigs(getApplication()),
                        deviceAuthResult = runCatching {
                            IronmeshPreferences.getDeviceAuthState(getApplication())
                        },
                        galleryViewMode = IronmeshPreferences.getGalleryViewMode(getApplication()),
                        titleLatencyMonitorSettings =
                            IronmeshPreferences.getTitleLatencyMonitorSettings(getApplication()),
                        themeAccentColorHex = IronmeshPreferences.getThemeAccentColor(getApplication()),
                    )
                }
            }
            val persisted = persistedResult.getOrElse { error ->
                uiState.value = uiState.value.copy(
                    persistedStateLoaded = true,
                    status = "App preferences unavailable: ${error.message}",
                )
                markPersistedStateLoaded()
                return@launch
            }
            val deviceAuth = persisted.deviceAuthResult.getOrDefault(DeviceAuthState())
            val loadedState = uiState.value.copy(
                persistedStateLoaded = true,
                syncProfiles = persisted.syncProfiles,
                deviceAuthState = deviceAuth,
                deviceLabelInput = deviceAuth.label.orEmpty(),
                galleryMode = persisted.galleryViewMode,
                titleLatencyMonitorSettings = persisted.titleLatencyMonitorSettings,
                themeAccentColorHex = persisted.themeAccentColorHex,
                status = persisted.deviceAuthResult.exceptionOrNull()?.let { error ->
                    "Device identity unavailable: ${error.message}"
                } ?: uiState.value.status,
            )
            val initialState = withContext(Dispatchers.IO) {
                loadedState.copy(
                    globalFolderSyncStatus = buildGlobalFolderSyncStatus(
                        state = loadedState,
                        runtimeStatus = loadedState.folderSyncStatus,
                        previousStatus = null,
                    ),
                )
            }
            uiState.value = initialState
            persistPreference {
                FolderSyncScheduler.reschedule(getApplication())
            }
            markPersistedStateLoaded()
        }
    }

    private fun markPersistedStateLoaded() {
        if (
            uiObservationGate.markPersistedStateLoaded() ==
            UiObservationTransition.START
        ) {
            startUiObservationJobs()
        }
    }

    private fun persistPreference(write: () -> Unit) {
        viewModelScope.launch {
            preferenceWriteMutex.withLock {
                runCatching {
                    withContext(Dispatchers.IO) {
                        write()
                    }
                }.onFailure { error ->
                    Log.e("MainViewModel", "Failed to persist app preference", error)
                }
            }
        }
    }

    private fun persistFolderSyncProfiles(profiles: List<FolderSyncConfig>) {
        persistPreference {
            IronmeshPreferences.setFolderSyncConfigs(getApplication(), profiles)
            FolderSyncScheduler.reschedule(getApplication())
        }
    }

    private fun stopUiObservation() {
        uiObservationGate.leaveForeground()
        stopUiObservationJobs()
    }

    private fun stopUiObservationJobs() {
        titleLatencyConfigurationJob?.cancel()
        titleLatencyConfigurationJob = null
        stopAppConnectionStatusMonitor()
        stopFolderSyncStatusMonitor()
        stopTitleLatencyStatusMonitor()
        stopConnectionRoutesMonitor()
    }

    private fun notifyManagedClientForegrounded() {
        viewModelScope.launch {
            val deviceAuth = runCatching { refreshPersistedDeviceAuthState() }
                .getOrElse { error ->
                    Log.w("MainViewModel", "Foreground identity refresh failed", error)
                    return@launch
                }
            val connectionInput = deviceAuth.connectionBootstrapJson()
            val clientIdentityJson = deviceAuth.toClientIdentityJson()
            if (connectionInput.isBlank() || clientIdentityJson.isNullOrBlank()) {
                return@launch
            }
            withContext(Dispatchers.IO) {
                runCatching {
                    repository.notifyForegrounded(
                        connectionInput = connectionInput,
                        serverCaPem = deviceAuth.serverCaPem?.takeIf { it.isNotBlank() },
                        clientIdentityJson = clientIdentityJson,
                    )
                }.onFailure { error ->
                    Log.w("MainViewModel", "Foreground route refresh hint failed", error)
                }
            }
        }
    }

    fun updateKey(value: String) {
        uiState.value = uiState.value.copy(key = value)
    }

    fun updateBootstrapInput(value: String) {
        uiState.value = uiState.value.copy(bootstrapInput = value)
    }

    fun updateDeviceLabelInput(value: String) {
        uiState.value = uiState.value.copy(deviceLabelInput = value)
    }

    fun updatePayload(value: String) {
        uiState.value = uiState.value.copy(payload = value)
    }

    fun updateThemeAccentColor(value: String) {
        val normalized = normalizeIronmeshAccentColorHex(value) ?: return
        uiState.value = uiState.value.copy(themeAccentColorHex = normalized)
        persistPreference {
            IronmeshPreferences.setThemeAccentColor(getApplication(), normalized)
        }
    }

    fun updateTitleLatencyMonitorEnabled(enabled: Boolean) {
        updateTitleLatencyMonitorSettings(
            uiState.value.titleLatencyMonitorSettings.copy(enabled = enabled),
        )
    }

    fun updateTitleLatencyMonitorPeriodSeconds(periodSeconds: Long) {
        updateTitleLatencyMonitorSettings(
            uiState.value.titleLatencyMonitorSettings.copy(periodSeconds = periodSeconds),
        )
    }

    private fun updateTitleLatencyMonitorSettings(settings: TitleLatencyMonitorSettings) {
        val normalized = settings.normalized()
        uiState.value = uiState.value.copy(titleLatencyMonitorSettings = normalized)
        persistPreference {
            IronmeshPreferences.setTitleLatencyMonitorSettings(getApplication(), normalized)
        }
        configureTitleLatencyMonitor()
    }

    fun putObject() {
        execute("Uploading object...") { deviceAuth ->
            val statusCode = repository.putObject(
                deviceAuth.connectionBootstrapJson(),
                uiState.value.key,
                uiState.value.payload,
                deviceAuth.serverCaPem?.takeIf { it.isNotBlank() },
                deviceAuth.toClientIdentityJson(),
            )
            "PUT ok: HTTP $statusCode"
        }
    }

    fun getObject() {
        execute("Downloading object...") { deviceAuth ->
            val body = repository.getObject(
                deviceAuth.connectionBootstrapJson(),
                uiState.value.key,
                serverCaPem = deviceAuth.serverCaPem?.takeIf { it.isNotBlank() },
                clientIdentityJson = deviceAuth.toClientIdentityJson(),
            )
            uiState.value = uiState.value.copy(objectBody = body)
            "GET ok: ${body.length} bytes"
        }
    }

    fun setStatus(message: String) {
        Log.i("MainViewModel", "status: $message")
        uiState.value = uiState.value.copy(status = message)
    }

    fun exportDiagnosticLog(destination: Uri) {
        Log.i("DiagnosticExport", "Preparing diagnostic log export")
        val snapshot = uiState.value
        viewModelScope.launch {
            runCatching {
                withContext(Dispatchers.IO) {
                    val rustLog = runCatching { repository.getDiagnosticLog() }
                        .getOrElse { error ->
                            "Rust diagnostic log unavailable: ${error.message}\n"
                        }
                    val logText = buildAndroidDiagnosticLogExport(
                        generatedAtUnixMs = System.currentTimeMillis(),
                        metadata = diagnosticMetadata(snapshot),
                        applicationLog = Log.renderText(),
                        rustLog = rustLog,
                    )
                    val resolver = getApplication<Application>().contentResolver
                    val output = resolver.openOutputStream(destination, "wt")
                        ?: error("The selected destination could not be opened")
                    output.bufferedWriter(Charsets.UTF_8).use { writer ->
                        writer.write(logText)
                    }
                }
            }.onSuccess {
                setStatus("Diagnostic log saved")
            }.onFailure { error ->
                Log.e("DiagnosticExport", "Failed to save diagnostic log", error)
                setStatus("Failed to save diagnostic log: ${error.message}")
            }
        }
    }

    private fun diagnosticMetadata(state: MainUiState): List<Pair<String, String>> {
        val deviceAuth = state.deviceAuthState
        val activeRoutes = state.connectionRoutes?.let { routes ->
            routes.endpoints
                .filter { endpoint ->
                    endpoint.lastUsedUnixMs?.let { lastUsed ->
                        lastUsed <= routes.generatedAtUnixMs &&
                            routes.generatedAtUnixMs - lastUsed <= 2_000L
                    } == true
                }
                .sortedByDescending { endpoint -> endpoint.lastUsedUnixMs }
        }.orEmpty()
        return listOf(
            "App version" to BuildConfig.LONG_VERSION,
            "Platform" to "Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})",
            "Device" to "${Build.MANUFACTURER} ${Build.MODEL}",
            "Cluster ID" to deviceAuth.clusterId.ifBlank { "unavailable" },
            "Device ID" to deviceAuth.deviceId.ifBlank { "unavailable" },
            "Device label" to deviceAuth.label.orEmpty().ifBlank { "unavailable" },
            "Visible section" to state.selectedSection.name,
            "UI status" to state.status,
            "Connection state" to "${state.appConnectionStatus.state}: ${state.appConnectionStatus.message}",
            "Active routes" to activeRoutes
                .joinToString(separator = "; ") { route -> "${route.pathKind}: ${route.locator}" }
                .ifBlank { "none" },
            "Connection diagnostics error" to state.connectionRoutesError.orEmpty().ifBlank { "none" },
            "Title latency" to state.titleLatencyStatus.toString(),
            "Sync profile count" to state.syncProfiles.size.toString(),
        )
    }

    fun updateNewSyncLabel(value: String) {
        uiState.value = uiState.value.copy(newSyncLabel = value)
    }

    fun updateNewSyncPrefix(value: String) {
        uiState.value = uiState.value.copy(newSyncPrefix = value)
    }

    fun updateNewSyncLocalFolder(value: String) {
        uiState.value = uiState.value.copy(
            newSyncLocalFolder = value,
            newSyncLocalFolderTreeUri = null,
        )
    }

    fun updateNewSyncLocalFolderSelection(
        localFolder: String,
        treeUri: String?,
    ) {
        uiState.value = uiState.value.copy(
            newSyncLocalFolder = localFolder,
            newSyncLocalFolderTreeUri = treeUri,
        )
    }

    fun updateNewSyncAllowWifi(value: Boolean) {
        uiState.value = uiState.value.copy(newSyncAllowWifi = value)
    }

    fun updateNewSyncAllowCellular(value: Boolean) {
        uiState.value = uiState.value.copy(newSyncAllowCellular = value)
    }

    fun updateNewSyncAllowOtherConnections(value: Boolean) {
        uiState.value = uiState.value.copy(newSyncAllowOtherConnections = value)
    }

    fun updateNewSyncAllowRoaming(value: Boolean) {
        uiState.value = uiState.value.copy(newSyncAllowRoaming = value)
    }

    fun updateNewSyncAllowedWifiSsids(value: String) {
        uiState.value = uiState.value.copy(newSyncAllowedWifiSsids = value)
    }

    fun selectSection(section: MainSection) {
        uiState.value = uiState.value.copy(selectedSection = section)
        if (section.isConnectionDiagnosticsSection()) {
            startConnectionRoutesMonitor()
        } else {
            stopConnectionRoutesMonitor()
        }
        if (section == MainSection.SYNC) {
            refreshExpandedFolderSyncHistory(force = true)
        }
        if (
            section == MainSection.LIBRARY &&
            uiState.value.galleryCollection == null &&
            uiState.value.galleryDirectories.isEmpty() &&
            !uiState.value.galleryLoading
        ) {
            refreshGallery()
        }
        if (section == MainSection.GALLERY_MAP && uiState.value.webUiSession == null && !uiState.value.loading) {
            startWebUi()
        }
    }

    fun refreshConnectionRoutes() {
        stopConnectionRoutesMonitor()
        viewModelScope.launch {
            loadConnectionRoutes(
                refresh = true,
                showLoading = true,
                statusOnFailure = true,
            )
            if (uiState.value.selectedSection.isConnectionDiagnosticsSection()) {
                startConnectionRoutesMonitor()
            }
        }
    }

    fun resetTimingMeasurement() {
        runTimingDiagnosticsOperation(
            actionDescription = "reset timing measurements",
            markInProgress = { current ->
                current.copy(
                    timingMeasurementResetting = true,
                    connectionRoutesError = null,
                )
            },
            request = { credentials ->
                repository.resetConnectionTimingMeasurement(
                    connectionInput = credentials.connectionInput,
                    serverCaPem = credentials.serverCaPem,
                    clientIdentityJson = credentials.clientIdentityJson,
                )
            },
            onSuccess = { current, snapshot ->
                current.copy(
                    connectionRoutes = snapshot,
                    connectionRoutesError = null,
                    connectionRoutesLastLoadedUnixMs = System.currentTimeMillis(),
                    timingMeasurementStartedUnixMs = System.currentTimeMillis(),
                    status = "Timing measurement reset. Run an action to capture new requests.",
                )
            },
        )
    }

    fun runTimingStoreIndexTest() {
        runTimingDiagnosticsOperation(
            actionDescription = "run the store-index timing test",
            markInProgress = { current ->
                current.copy(
                    timingStoreIndexTestRunning = true,
                    connectionRoutesError = null,
                )
            },
            request = { credentials ->
                repository.runConnectionTimingStoreIndexTest(
                    connectionInput = credentials.connectionInput,
                    serverCaPem = credentials.serverCaPem,
                    clientIdentityJson = credentials.clientIdentityJson,
                )
            },
            onSuccess = { current, snapshot ->
                current.copy(
                    connectionRoutes = snapshot,
                    connectionRoutesError = null,
                    connectionRoutesLastLoadedUnixMs = System.currentTimeMillis(),
                    status = "Store-index timing test completed.",
                )
            },
        )
    }

    fun refreshGallery() {
        ThumbnailBitmapCache.clear()
        val request = currentGalleryRequest(pageSize = uiState.value.galleryCollection?.pageSize ?: GALLERY_PAGE_SIZE)
        val requestVersion = nextGalleryRequestVersion()
        pinnedGalleryItemIndex = null
        uiState.value = uiState.value.withGalleryRefreshStarted()
        viewModelScope.launch {
            runCatching {
                val deviceAuth = refreshPersistedDeviceAuthState()
                withContext(Dispatchers.IO) {
                    loadGalleryState(request, deviceAuth)
                }
            }
                .onSuccess { snapshot ->
                    if (!isCurrentGalleryRequest(requestVersion)) {
                        return@onSuccess
                    }
                    val firstPage = snapshot.firstPageItems
                    uiState.value = uiState.value.copy(
                        galleryCollection = snapshot.collection,
                        galleryPages = if (firstPage.isEmpty()) {
                            emptyMap()
                        } else {
                            mapOf(
                                0 to GalleryPageState(
                                    status = GalleryPageStatus.READY,
                                    items = firstPage,
                                ),
                            )
                        },
                        galleryDirectories = snapshot.directories,
                        galleryBreadcrumbs = snapshot.breadcrumbs,
                        galleryCurrentDirectoryDocumentId = snapshot.currentDirectoryDocumentId,
                        galleryCurrentDirectoryPath = snapshot.currentDirectoryPath,
                        galleryLoading = false,
                        galleryError = null,
                        status = when (snapshot.mode) {
                            GalleryViewMode.FLATTENED_ALL_IMAGES ->
                                "Gallery loaded: ${snapshot.collection.totalItemCount} images"
                            GalleryViewMode.CURRENT_DIRECTORY ->
                                "Gallery loaded: ${snapshot.collection.totalItemCount} images, ${snapshot.directories.size} folders in ${snapshot.currentDirectoryPath}"
                        },
                    )
                    updateVisibleGalleryPages(setOf(0))
                }
                .onFailure { error ->
                    if (!isCurrentGalleryRequest(requestVersion)) {
                        return@onFailure
                    }
                    uiState.value = uiState.value.withGalleryRefreshFailure(error)
                }
        }
    }

    fun updateGallerySort(sort: GallerySortOption) {
        if (uiState.value.gallerySort == sort) {
            return
        }
        uiState.value = uiState.value.copy(
            gallerySort = sort,
            galleryCollection = null,
            galleryPages = emptyMap(),
        )
        refreshGallery()
    }

    fun updateGalleryViewMode(mode: GalleryViewMode) {
        if (uiState.value.galleryMode == mode) {
            return
        }
        uiState.value = uiState.value.copy(
            galleryMode = mode,
            galleryCollection = null,
            galleryPages = emptyMap(),
            galleryDirectories = emptyList(),
            galleryBreadcrumbs = emptyList(),
            galleryCurrentDirectoryDocumentId = GALLERY_ROOT_DOCUMENT_ID,
            galleryCurrentDirectoryPath = GALLERY_ROOT_PATH,
        )
        persistPreference {
            IronmeshPreferences.setGalleryViewMode(getApplication(), mode)
        }
        refreshGallery()
    }

    fun openGalleryDirectory(directory: GalleryDirectoryItem) {
        val nextBreadcrumbs = if (uiState.value.galleryMode == GalleryViewMode.CURRENT_DIRECTORY) {
            uiState.value.galleryBreadcrumbs + GalleryBreadcrumbItem(
                documentId = directory.documentId,
                label = directory.displayName,
                pathLabel = directory.pathLabel,
            )
        } else {
            emptyList()
        }
        uiState.value = uiState.value.copy(
            galleryCollection = null,
            galleryPages = emptyMap(),
            galleryBreadcrumbs = nextBreadcrumbs,
            galleryCurrentDirectoryDocumentId = directory.documentId,
            galleryCurrentDirectoryPath = directory.pathLabel,
        )
        refreshGallery()
    }

    fun navigateGalleryToRoot() {
        if (
            uiState.value.galleryCurrentDirectoryDocumentId == GALLERY_ROOT_DOCUMENT_ID &&
            uiState.value.galleryCurrentDirectoryPath == GALLERY_ROOT_PATH
        ) {
            return
        }
        uiState.value = uiState.value.copy(
            galleryCollection = null,
            galleryPages = emptyMap(),
            galleryBreadcrumbs = emptyList(),
            galleryCurrentDirectoryDocumentId = GALLERY_ROOT_DOCUMENT_ID,
            galleryCurrentDirectoryPath = GALLERY_ROOT_PATH,
        )
        refreshGallery()
    }

    fun navigateGalleryUp() {
        if (uiState.value.galleryBreadcrumbs.isEmpty()) {
            navigateGalleryToRoot()
            return
        }
        val nextBreadcrumbs = uiState.value.galleryBreadcrumbs.dropLast(1)
        val nextCurrent = nextBreadcrumbs.lastOrNull()
        uiState.value = uiState.value.copy(
            galleryCollection = null,
            galleryPages = emptyMap(),
            galleryBreadcrumbs = nextBreadcrumbs,
            galleryCurrentDirectoryDocumentId = nextCurrent?.documentId ?: GALLERY_ROOT_DOCUMENT_ID,
            galleryCurrentDirectoryPath = nextCurrent?.pathLabel ?: GALLERY_ROOT_PATH,
        )
        refreshGallery()
    }

    fun navigateGalleryToBreadcrumb(index: Int) {
        if (index < 0) {
            navigateGalleryToRoot()
            return
        }
        val nextBreadcrumbs = uiState.value.galleryBreadcrumbs.take(index + 1)
        val nextCurrent = nextBreadcrumbs.lastOrNull()
        uiState.value = uiState.value.copy(
            galleryCollection = null,
            galleryPages = emptyMap(),
            galleryBreadcrumbs = nextBreadcrumbs,
            galleryCurrentDirectoryDocumentId = nextCurrent?.documentId ?: GALLERY_ROOT_DOCUMENT_ID,
            galleryCurrentDirectoryPath = nextCurrent?.pathLabel ?: GALLERY_ROOT_PATH,
        )
        refreshGallery()
    }

    fun galleryItemAt(index: Int): GalleryImageItem? {
        val collection = uiState.value.galleryCollection ?: return null
        if (index < 0 || index >= collection.totalItemCount) {
            return null
        }
        val page = uiState.value.galleryPages[pageIndexForGalleryEntry(index, collection.pageSize)]
            ?: return null
        if (page.status != GalleryPageStatus.READY) {
            return null
        }
        return page.items[galleryEntryWithinPage(index, collection.pageSize)]
    }

    fun pinGalleryItem(index: Int?) {
        pinnedGalleryItemIndex = index?.takeIf { it >= 0 }
    }

    fun ensureGalleryItemLoaded(index: Int) {
        val collection = uiState.value.galleryCollection ?: return
        if (index < 0 || index >= collection.totalItemCount) {
            return
        }
        ensureGalleryPageLoaded(pageIndexForGalleryEntry(index, collection.pageSize))
    }

    fun retryGalleryPage(pageIndex: Int) {
        ensureGalleryPageLoaded(pageIndex, force = true)
    }

    fun updateVisibleGalleryPages(visiblePageIndices: Set<Int>) {
        val collection = uiState.value.galleryCollection ?: return
        if (collection.pageCount == 0) {
            return
        }

        val visible = visiblePageIndices
            .filter { it in 0 until collection.pageCount }
            .sorted()
        val pinnedPageIndex = pinnedGalleryItemIndex?.let {
            pageIndexForGalleryEntry(it, collection.pageSize)
        }
        val minVisible = visible.firstOrNull() ?: pinnedPageIndex ?: 0
        val maxVisible = visible.lastOrNull() ?: pinnedPageIndex ?: 0
        val rangeMin = pinnedPageIndex?.let { minOf(minVisible, it) } ?: minVisible
        val rangeMax = pinnedPageIndex?.let { maxOf(maxVisible, it) } ?: maxVisible
        val preloadStart = (rangeMin - GALLERY_PAGE_PRELOAD_RADIUS).coerceAtLeast(0)
        val preloadEnd = (rangeMax + GALLERY_PAGE_PRELOAD_RADIUS).coerceAtMost(collection.pageCount - 1)
        val keepStart = (rangeMin - GALLERY_PAGE_KEEP_RADIUS).coerceAtLeast(0)
        val keepEnd = (rangeMax + GALLERY_PAGE_KEEP_RADIUS).coerceAtMost(collection.pageCount - 1)

        for (pageIndex in preloadStart..preloadEnd) {
            ensureGalleryPageLoaded(pageIndex)
        }

        val nextPages = uiState.value.galleryPages.filterKeys { pageIndex ->
            pageIndex in keepStart..keepEnd ||
                uiState.value.galleryPages[pageIndex]?.status == GalleryPageStatus.LOADING
        }
        if (nextPages.size != uiState.value.galleryPages.size) {
            uiState.value = uiState.value.copy(galleryPages = nextPages)
        }
    }

    private fun ensureGalleryPageLoaded(
        pageIndex: Int,
        force: Boolean = false,
    ) {
        val currentState = uiState.value
        val collection = currentState.galleryCollection ?: return
        if (pageIndex < 0 || pageIndex >= collection.pageCount) {
            return
        }

        val existing = currentState.galleryPages[pageIndex]
        if (!force && (existing?.status == GalleryPageStatus.READY || existing?.status == GalleryPageStatus.LOADING)) {
            return
        }

        val request = currentGalleryRequest(pageSize = collection.pageSize)
        val requestVersion = galleryRequestVersion
        uiState.value = currentState.copy(
            galleryPages = currentState.galleryPages.toMutableMap().apply {
                put(
                    pageIndex,
                    GalleryPageState(
                        status = GalleryPageStatus.LOADING,
                        items = existing?.items.orEmpty(),
                    ),
                )
            },
        )

        viewModelScope.launch {
            runCatching {
                val deviceAuth = refreshPersistedDeviceAuthState()
                withContext(Dispatchers.IO) {
                    loadGalleryPage(
                        request = request,
                        pageIndex = pageIndex,
                        deviceAuth = deviceAuth,
                    )
                }
            }
                .onSuccess { response ->
                    if (!isCurrentGalleryRequest(requestVersion)) {
                        return@onSuccess
                    }
                    val items = response.entries.mapNotNull(::galleryImageItemFromEntry)
                    val nextCollection = collection.copy(
                        totalItemCount = response.total_entry_count.coerceAtLeast(items.size),
                        pageCount = resolveGalleryPageCount(
                            totalItemCount = response.total_entry_count.coerceAtLeast(items.size),
                            pageSize = collection.pageSize,
                        ),
                    )
                    uiState.value = uiState.value.copy(
                        galleryCollection = nextCollection,
                        galleryPages = uiState.value.galleryPages.toMutableMap().apply {
                            put(
                                pageIndex,
                                GalleryPageState(
                                    status = GalleryPageStatus.READY,
                                    items = items,
                                ),
                            )
                        },
                    )
                }
                .onFailure { error ->
                    if (!isCurrentGalleryRequest(requestVersion)) {
                        return@onFailure
                    }
                    uiState.value = uiState.value.copy(
                        galleryPages = uiState.value.galleryPages.toMutableMap().apply {
                            put(
                                pageIndex,
                                GalleryPageState(
                                    status = GalleryPageStatus.ERROR,
                                    error = error.message ?: "Failed to load gallery page",
                                ),
                            )
                        },
                    )
                }
        }
    }

    fun addFolderSyncProfile(): FolderSyncNetworkPolicy? {
        val localFolder = uiState.value.newSyncLocalFolder.trim()
        if (localFolder.isBlank()) {
            setStatus("Error: Local folder path is required")
            return null
        }

        val prefix = uiState.value.newSyncPrefix.trim().trim('/').replace('\\', '/')
        val label = uiState.value.newSyncLabel.trim().ifBlank {
            localFolder.substringAfterLast('/').ifBlank { "Sync Profile" }
        }
        val networkPolicy = FolderSyncNetworkPolicy(
            allowWifi = uiState.value.newSyncAllowWifi,
            allowCellular = uiState.value.newSyncAllowCellular,
            allowOtherConnections = uiState.value.newSyncAllowOtherConnections,
            allowRoaming = uiState.value.newSyncAllowRoaming,
            allowedWifiSsids = parseAllowedWifiSsidsInput(uiState.value.newSyncAllowedWifiSsids),
        ).normalized()
        if (!networkPolicy.hasAnyAllowedTransport()) {
            setStatus("Error: Select at least one allowed network type")
            return null
        }

        val profile = FolderSyncConfig(
            id = UUID.randomUUID().toString(),
            label = label,
            prefix = prefix,
            localFolder = localFolder,
            localFolderTreeUri = uiState.value.newSyncLocalFolderTreeUri?.takeIf { it.isNotBlank() },
            depth = 64,
            enabled = true,
            networkPolicy = networkPolicy,
        )

        val updated = uiState.value.syncProfiles + profile
        uiState.value = uiState.value.copy(
            syncProfiles = updated,
            newSyncLabel = "",
            newSyncPrefix = "",
            newSyncLocalFolder = "",
            newSyncLocalFolderTreeUri = null,
            newSyncAllowWifi = true,
            newSyncAllowCellular = true,
            newSyncAllowOtherConnections = true,
            newSyncAllowRoaming = false,
            newSyncAllowedWifiSsids = "",
            status = "Added sync profile '${profile.label}'",
        )
        persistFolderSyncProfiles(updated)
        return profile.networkPolicy
    }

    fun setFolderSyncProfileEnabled(profileId: String, enabled: Boolean) {
        val updated = uiState.value.syncProfiles.map { profile ->
            if (profile.id == profileId) profile.copy(enabled = enabled) else profile
        }
        uiState.value = uiState.value.copy(syncProfiles = updated)
        persistFolderSyncProfiles(updated)
    }

    fun removeFolderSyncProfile(profileId: String) {
        val updated = uiState.value.syncProfiles.filterNot { it.id == profileId }
        val updatedHistory = uiState.value.folderSyncHistory.toMutableMap().apply {
            remove(profileId)
        }
        uiState.value = uiState.value.copy(
            syncProfiles = updated,
            folderSyncHistory = updatedHistory,
            status = "Removed sync profile",
        )
        persistFolderSyncProfiles(updated)
    }

    fun updateFolderSyncProfileNetworkPolicy(
        profileId: String,
        networkPolicy: FolderSyncNetworkPolicy,
    ): Boolean {
        val normalizedPolicy = networkPolicy.normalized()
        if (!normalizedPolicy.hasAnyAllowedTransport()) {
            setStatus("Error: Select at least one allowed network type")
            return false
        }

        val targetProfile = uiState.value.syncProfiles.firstOrNull { profile -> profile.id == profileId }
            ?: return false
        val updated = uiState.value.syncProfiles.map { profile ->
            if (profile.id == profileId) {
                profile.copy(networkPolicy = normalizedPolicy)
            } else {
                profile
            }
        }
        uiState.value = uiState.value.copy(
            syncProfiles = updated,
            status = "Updated network rules for '${targetProfile.label}'",
        )
        persistFolderSyncProfiles(updated)
        return true
    }

    fun runFolderSyncNow() {
        val enabledProfiles = uiState.value.syncProfiles.filter { profile -> profile.enabled }
        if (enabledProfiles.isEmpty()) {
            setStatus("No enabled sync profile is configured")
            return
        }
        val eligibleProfiles = FolderSyncNetworkGate
            .evaluateProfiles(getApplication(), enabledProfiles)
        val firstBlocked = eligibleProfiles.firstOrNull { evaluation -> !evaluation.decision.allowed }
        if (eligibleProfiles.none { evaluation -> evaluation.decision.allowed }) {
            val reason = firstBlocked?.decision?.reason ?: "No profile is allowed on the current network"
            setStatus("Sync skipped: $reason")
            return
        }

        FolderSyncScheduler.runNow(getApplication())
        refreshExpandedFolderSyncHistory(force = true)
        setStatus("Requested immediate folder reconciliation")
    }

    fun retryAppConnection() {
        setStatus("Checking app connection")
        viewModelScope.launch {
            val deviceAuth = runCatching { refreshPersistedDeviceAuthState() }
                .getOrElse { error ->
                    setStatus("Device identity unavailable: ${error.message}")
                    return@launch
                }
            val connectionInput = deviceAuth.connectionBootstrapJson()
            val clientIdentityJson = deviceAuth.toClientIdentityJson()
            if (connectionInput.isBlank() || clientIdentityJson.isNullOrBlank()) {
                setStatus("Enroll this device before checking the connection")
                return@launch
            }
            val result = withContext(Dispatchers.IO) {
                runCatching {
                    repository.notifyNetworkChanged(
                        connectionInput = connectionInput,
                        serverCaPem = deviceAuth.serverCaPem?.takeIf { it.isNotBlank() },
                        clientIdentityJson = clientIdentityJson,
                    )
                    repository.storeIndex(
                        connectionInput = connectionInput,
                        depth = 1,
                        serverCaPem = deviceAuth.serverCaPem?.takeIf { it.isNotBlank() },
                        clientIdentityJson = clientIdentityJson,
                    )
                }
            }
            uiState.value = uiState.value.copy(
                status = result.fold(
                    onSuccess = { "Connection check succeeded" },
                    onFailure = { error -> "Connection check failed: ${error.message}" },
                ),
            )
        }
    }

    fun toggleFolderSyncHistory(profileId: String) {
        val current = uiState.value.folderSyncHistory[profileId] ?: FolderSyncHistoryState()
        updateFolderSyncHistoryState(profileId) { historyState ->
            historyState.copy(
                expanded = !current.expanded,
                error = if (current.expanded) null else historyState.error,
            )
        }
        val next = uiState.value.folderSyncHistory[profileId] ?: FolderSyncHistoryState()
        if (next.expanded && (next.records.isEmpty() || isHistoryStale(next))) {
            refreshFolderSyncHistory(profileId)
        }
    }

    fun setFolderSyncHistoryFilter(
        profileId: String,
        filter: FolderSyncActivityFilter,
    ) {
        updateFolderSyncHistoryState(profileId) { historyState ->
            historyState.copy(filter = filter)
        }
    }

    fun loadMoreFolderSyncHistory(profileId: String) {
        val current = uiState.value.folderSyncHistory[profileId] ?: return
        if (current.loading || current.nextBeforeId == null) {
            return
        }
        refreshFolderSyncHistory(
            profileId = profileId,
            beforeId = current.nextBeforeId,
            append = true,
        )
    }

    fun startWebUi() {
        webUiSessionBackgroundGrace.cancelScheduledStop()
        uiState.value.webUiSession?.let { session ->
            EmbeddedWebUiSessionRegistry.activate(session)
            uiState.value = uiState.value.copy(status = "Web UI ready.")
            return
        }
        uiState.value = uiState.value.copy(
            loading = true,
            webUiSession = null,
            status = "Starting Web UI...",
        )
        viewModelScope.launch {
            val deviceAuth = runCatching { refreshPersistedDeviceAuthState() }
                .getOrElse { error ->
                    uiState.value = uiState.value.copy(
                        loading = false,
                        webUiSession = null,
                        status = "Device identity unavailable: ${error.message}",
                    )
                    return@launch
                }
            val connectionInput = deviceAuth.connectionBootstrapJson()
            val clientIdentityJson = deviceAuth.toClientIdentityJson()
            if (connectionInput.isBlank() || clientIdentityJson.isNullOrBlank()) {
                uiState.value = uiState.value.copy(
                    loading = false,
                    webUiSession = null,
                    status = "Enroll this device before opening the Web UI.",
                )
                return@launch
            }
            val result = runCatching {
                withContext(Dispatchers.IO) {
                    repository.startWebUi(
                        connectionInput,
                        deviceAuth.serverCaPem?.takeIf { it.isNotBlank() },
                        clientIdentityJson,
                    )
                }
            }
            runCatching { refreshPersistedDeviceAuthState() }
                .onFailure { error ->
                    uiState.value = uiState.value.copy(
                        loading = false,
                        status = "Device identity unavailable: ${error.message}",
                    )
                    return@launch
                }
            result
                .onSuccess { session ->
                    EmbeddedWebUiSessionRegistry.activate(session)
                    uiState.value = uiState.value.copy(
                        loading = false,
                        webUiSession = session,
                        status = "Web UI ready.",
                    )
                }
                .onFailure { error ->
                    uiState.value = uiState.value.copy(
                        loading = false,
                        status = "Error: ${error.message}",
                    )
                }
        }
    }

    private fun stopWebUiAfterBackgroundGrace() {
        if (uiState.value.webUiSession == null) {
            return
        }
        EmbeddedWebUiSessionRegistry.clear()
        repository.stopWebUi()
        uiState.value = uiState.value.copy(webUiSession = null)
    }

    fun enrollDevice() {
        val bootstrapJson = uiState.value.bootstrapInput.trim()
        if (bootstrapJson.isBlank()) {
            val detail = "Bootstrap claim or bundle is required"
            uiState.value = uiState.value.copy(
                status = "Error: $detail",
                enrollmentDiagnostics = newEnrollmentDiagnostics().withEnrollmentDiagnosticStatus(
                    stepId = EnrollmentDiagnosticStepId.BOOTSTRAP,
                    status = EnrollmentDiagnosticStepStatus.FAILED,
                    detail = detail,
                ),
            )
            return
        }

        val label = uiState.value.deviceLabelInput.trim().takeIf { it.isNotBlank() }
        val existingDeviceId = uiState.value.deviceAuthState.deviceId.takeIf { it.isNotBlank() }
        uiState.value = uiState.value.copy(
            loading = true,
            status = "Enrolling device...",
            enrollmentDiagnostics = newEnrollmentDiagnostics().withEnrollmentDiagnosticStatus(
                stepId = EnrollmentDiagnosticStepId.BOOTSTRAP,
                status = EnrollmentDiagnosticStepStatus.IN_PROGRESS,
            ),
        )
        viewModelScope.launch {
            val authState = try {
                withContext(Dispatchers.IO) {
                    repository.enrollWithBootstrap(
                        bootstrapJson = bootstrapJson,
                        deviceId = existingDeviceId,
                        label = label,
                    )
                }
            } catch (error: Throwable) {
                finishEnrollmentWithError(EnrollmentDiagnosticStepId.BOOTSTRAP, error)
                return@launch
            }

            uiState.value = uiState.value.copy(
                status = "Verifying enrollment...",
                enrollmentDiagnostics = uiState.value.enrollmentDiagnostics
                    .withEnrollmentDiagnosticStatus(
                        stepId = EnrollmentDiagnosticStepId.BOOTSTRAP,
                        status = EnrollmentDiagnosticStepStatus.SUCCEEDED,
                    )
                    .withEnrollmentDiagnosticStatus(
                        stepId = EnrollmentDiagnosticStepId.VERIFY_ACCESS,
                        status = EnrollmentDiagnosticStepStatus.IN_PROGRESS,
                        detail = enrollmentVerificationProgressDetail(
                            elapsedMs = 0L,
                            connectionRoutes = null,
                        ),
                    ),
            )
            val verificationStartedAtNanos = System.nanoTime()
            startEnrollmentVerificationMonitor(authState, verificationStartedAtNanos)
            Log.i(ENROLLMENT_LOG_TAG, "signed access verification started")
            val verification: EnrollmentAccessVerification
            try {
                verification = withContext(Dispatchers.IO) {
                    repository.verifyEnrollmentAccess(authState)
                }
            } catch (error: Throwable) {
                finishEnrollmentWithError(EnrollmentDiagnosticStepId.VERIFY_ACCESS, error)
                return@launch
            }
            stopEnrollmentVerificationMonitor()
            Log.i(
                ENROLLMENT_LOG_TAG,
                enrollmentVerificationSuccessDetail(verification),
            )

            uiState.value = uiState.value.copy(
                status = "Saving device identity...",
                enrollmentDiagnostics = uiState.value.enrollmentDiagnostics
                    .withEnrollmentDiagnosticStatus(
                        stepId = EnrollmentDiagnosticStepId.VERIFY_ACCESS,
                        status = EnrollmentDiagnosticStepStatus.SUCCEEDED,
                        detail = enrollmentVerificationSuccessDetail(verification),
                    )
                    .withEnrollmentDiagnosticStatus(
                        stepId = EnrollmentDiagnosticStepId.SAVE_IDENTITY,
                        status = EnrollmentDiagnosticStepStatus.IN_PROGRESS,
                    ),
            )
            try {
                withContext(Dispatchers.IO) {
                    EmbeddedWebUiSessionRegistry.clear()
                    repository.stopWebUi()
                    IronmeshPreferences.setDeviceAuthState(getApplication(), authState)
                    FolderSyncScheduler.reschedule(getApplication())
                }
            } catch (error: Throwable) {
                finishEnrollmentWithError(EnrollmentDiagnosticStepId.SAVE_IDENTITY, error)
                return@launch
            }

            stopConnectionRoutesMonitor()
            uiState.value = uiState.value.copy(
                loading = false,
                deviceAuthState = authState,
                bootstrapInput = "",
                deviceLabelInput = authState.label.orEmpty(),
                enrollmentDiagnostics = uiState.value.enrollmentDiagnostics
                    .withEnrollmentDiagnosticStatus(
                        stepId = EnrollmentDiagnosticStepId.SAVE_IDENTITY,
                        status = EnrollmentDiagnosticStepStatus.SUCCEEDED,
                    ),
                connectionRoutes = null,
                connectionRoutesError = null,
                connectionRoutesLastLoadedUnixMs = 0L,
                selectedSection = MainSection.HOME,
                webUiSession = null,
                status = "Device enrolled: ${authState.deviceId}",
            )
            if (uiState.value.titleLatencyMonitorSettings.enabled) {
                configureTitleLatencyMonitor()
            }
        }
    }

    private fun finishEnrollmentWithError(
        stepId: EnrollmentDiagnosticStepId,
        error: Throwable,
    ) {
        stopEnrollmentVerificationMonitor()
        val detail = enrollmentDiagnosticErrorDetail(error)
        if (stepId == EnrollmentDiagnosticStepId.VERIFY_ACCESS) {
            Log.e(ENROLLMENT_LOG_TAG, detail, error)
        }
        uiState.value = uiState.value.copy(
            loading = false,
            status = "Error: $detail",
            enrollmentDiagnostics = uiState.value.enrollmentDiagnostics.withEnrollmentDiagnosticStatus(
                stepId = stepId,
                status = EnrollmentDiagnosticStepStatus.FAILED,
                detail = detail,
            ),
        )
    }

    private fun execute(
        loadingMessage: String,
        action: suspend (DeviceAuthState) -> String,
    ) {
        uiState.value = uiState.value.copy(loading = true, status = loadingMessage)
        viewModelScope.launch {
            val deviceAuth = runCatching { refreshPersistedDeviceAuthState() }
                .getOrElse { error ->
                    uiState.value = uiState.value.copy(
                        loading = false,
                        status = "Device identity unavailable: ${error.message}",
                    )
                    return@launch
                }
            val result = runCatching { action(deviceAuth) }
            runCatching { refreshPersistedDeviceAuthState() }
                .onFailure { error ->
                    uiState.value = uiState.value.copy(
                        loading = false,
                        status = "Device identity unavailable: ${error.message}",
                    )
                    return@launch
                }
            result
                .onSuccess { message ->
                    uiState.value = uiState.value.copy(loading = false, status = message)
                }
                .onFailure { error ->
                    uiState.value = uiState.value.copy(
                        loading = false,
                        status = "Error: ${error.message}",
                    )
                }
        }
    }

    private fun startFolderSyncStatusMonitor() {
        if (
            !uiObservationGate.observationJobsActive ||
            folderSyncStatusMonitorJob?.isActive == true
        ) {
            return
        }
        folderSyncStatusMonitorJob = viewModelScope.launch {
            var historyRefreshTick = 0
            while (isActive) {
                val stateSnapshot = uiState.value
                val result = withContext(Dispatchers.IO) {
                    val status = runCatching { repository.getContinuousFolderSyncStatus() }
                        .getOrDefault(FolderSyncServiceStatus())
                    FolderSyncPollResult(
                        folderSyncStatus = status,
                        globalFolderSyncStatus = buildGlobalFolderSyncStatus(
                            state = stateSnapshot,
                            runtimeStatus = status,
                            previousStatus = stateSnapshot.globalFolderSyncStatus,
                        ),
                    )
                }
                val currentState = uiState.value
                if (canApplyFolderSyncPollResult(stateSnapshot, currentState)) {
                    uiState.value = currentState.withFolderSyncPollResult(result)
                }
                historyRefreshTick += 1
                if (historyRefreshTick >= 5) {
                    historyRefreshTick = 0
                    refreshExpandedFolderSyncHistory()
                }
                delay(1_000)
            }
        }
    }

    private fun stopFolderSyncStatusMonitor() {
        folderSyncStatusMonitorJob?.cancel()
        folderSyncStatusMonitorJob = null
    }

    private fun startAppConnectionStatusMonitor() {
        if (
            !uiObservationGate.observationJobsActive ||
            appConnectionStatusMonitorJob?.isActive == true
        ) {
            return
        }
        appConnectionStatusMonitorJob = viewModelScope.launch {
            RustPreferencesBridge.appConnectionStatus()
                .filterNotNull()
                .collect { connectionStatus ->
                    if (uiState.value.appConnectionStatus != connectionStatus) {
                        uiState.value = uiState.value.copy(
                            appConnectionStatus = connectionStatus,
                        )
                    }
                }
        }
    }

    private fun stopAppConnectionStatusMonitor() {
        appConnectionStatusMonitorJob?.cancel()
        appConnectionStatusMonitorJob = null
    }

    private fun buildGlobalFolderSyncStatus(
        state: MainUiState,
        runtimeStatus: FolderSyncServiceStatus,
        previousStatus: GlobalFolderSyncStatus? = state.globalFolderSyncStatus,
    ): GlobalFolderSyncStatus {
        val enabledProfiles = state.syncProfiles.filter { profile -> profile.enabled }
        val enrollmentReady = state.deviceAuthState.hasClientIdentity()
        val blockedProfiles = if (enrollmentReady) {
            runCatching {
                FolderSyncNetworkGate
                    .evaluateProfiles(getApplication(), enabledProfiles)
                    .filterNot { evaluation -> evaluation.decision.allowed }
            }.getOrDefault(emptyList())
        } else {
            emptyList()
        }
        val execution = FolderSyncExecutionCoordinator.snapshot()
        return mergeGlobalFolderSyncStatus(
            configs = state.syncProfiles,
            runtimeStatus = runtimeStatus,
            previousStatus = previousStatus,
            blockedProfileCount = blockedProfiles.size,
            blockedReason = blockedProfiles.firstOrNull()?.decision?.reason,
            enrollmentReady = enrollmentReady,
            oneShotRunning = execution.oneShotRunning,
            oneShotProfileLabel = execution.oneShotProfileLabel,
        )
    }

    private fun refreshExpandedFolderSyncHistory(force: Boolean = false) {
        if (uiState.value.selectedSection != MainSection.SYNC) {
            return
        }
        val historyStates = uiState.value.folderSyncHistory
        historyStates.forEach { (profileId, historyState) ->
            if (!historyState.expanded || historyState.loading) {
                return@forEach
            }
            if (force || isHistoryStale(historyState)) {
                refreshFolderSyncHistory(profileId)
            }
        }
    }

    private fun refreshFolderSyncHistory(
        profileId: String,
        beforeId: Long? = null,
        append: Boolean = false,
    ) {
        val profile = uiState.value.syncProfiles.firstOrNull { it.id == profileId } ?: return
        val existing = uiState.value.folderSyncHistory[profileId] ?: FolderSyncHistoryState()
        if (existing.loading) {
            return
        }

        val limit = if (append) {
            FOLDER_SYNC_HISTORY_PAGE_SIZE
        } else {
            existing.records.size.coerceAtLeast(FOLDER_SYNC_HISTORY_PAGE_SIZE)
        }
        updateFolderSyncHistoryState(profileId) { historyState ->
            historyState.copy(
                expanded = true,
                loading = true,
                error = null,
            )
        }

        viewModelScope.launch {
            val connectionInput = runCatching {
                refreshPersistedDeviceAuthState().connectionBootstrapJson()
            }.getOrElse { error ->
                updateFolderSyncHistoryState(profileId) { historyState ->
                    historyState.copy(
                        expanded = true,
                        loading = false,
                        error = error.message ?: "Device identity is unavailable",
                    )
                }
                uiState.value = uiState.value.copy(
                    status = "Device identity unavailable: ${error.message}",
                )
                return@launch
            }
            runCatching {
                withContext(Dispatchers.IO) {
                    repository.getFolderSyncModificationHistory(
                        connectionInput = connectionInput,
                        profile = profile,
                        limit = limit,
                        beforeId = beforeId,
                    )
                }
            }
                .onSuccess { history ->
                    updateFolderSyncHistoryState(profileId) { historyState ->
                        val nextRecords = if (append) {
                            (historyState.records + history.records).distinctBy { record -> record.id }
                        } else {
                            history.records
                        }
                        historyState.copy(
                            expanded = true,
                            records = nextRecords,
                            nextBeforeId = history.nextBeforeId,
                            loading = false,
                            error = null,
                            lastLoadedElapsedRealtimeMs = SystemClock.elapsedRealtime(),
                        )
                    }
                }
                .onFailure { error ->
                    updateFolderSyncHistoryState(profileId) { historyState ->
                        historyState.copy(
                            expanded = true,
                            loading = false,
                            error = error.message ?: "Failed to load recent activity",
                        )
                    }
                }
        }
    }

    private fun updateFolderSyncHistoryState(
        profileId: String,
        transform: (FolderSyncHistoryState) -> FolderSyncHistoryState,
    ) {
        val current = uiState.value.folderSyncHistory[profileId] ?: FolderSyncHistoryState()
        val updated = uiState.value.folderSyncHistory.toMutableMap().apply {
            put(profileId, transform(current))
        }
        uiState.value = uiState.value.copy(folderSyncHistory = updated)
    }

    private fun isHistoryStale(historyState: FolderSyncHistoryState): Boolean {
        return SystemClock.elapsedRealtime() - historyState.lastLoadedElapsedRealtimeMs >=
            FOLDER_SYNC_HISTORY_REFRESH_MS
    }

    private data class GalleryRequest(
        val mode: GalleryViewMode,
        val currentDirectoryDocumentId: String,
        val currentDirectoryPath: String,
        val breadcrumbs: List<GalleryBreadcrumbItem>,
        val sort: GallerySortOption,
        val pageSize: Int,
    )

    private data class GalleryLoadSnapshot(
        val mode: GalleryViewMode,
        val currentDirectoryDocumentId: String,
        val currentDirectoryPath: String,
        val breadcrumbs: List<GalleryBreadcrumbItem>,
        val directories: List<GalleryDirectoryItem>,
        val collection: GalleryCollectionState,
        val firstPageItems: List<GalleryImageItem>,
    )

    private fun currentGalleryRequest(pageSize: Int): GalleryRequest {
        val current = uiState.value
        return GalleryRequest(
            mode = current.galleryMode,
            currentDirectoryDocumentId = current.galleryCurrentDirectoryDocumentId,
            currentDirectoryPath = current.galleryCurrentDirectoryPath,
            breadcrumbs = current.galleryBreadcrumbs,
            sort = current.gallerySort,
            pageSize = pageSize.coerceAtLeast(1),
        )
    }

    private suspend fun loadGalleryState(
        request: GalleryRequest,
        deviceAuth: DeviceAuthState,
    ): GalleryLoadSnapshot {
        return when (request.mode) {
            GalleryViewMode.FLATTENED_ALL_IMAGES ->
                loadFlattenedGalleryState(request, deviceAuth)
            GalleryViewMode.CURRENT_DIRECTORY ->
                loadCurrentDirectoryGalleryState(request, deviceAuth)
        }
    }

    private suspend fun loadFlattenedGalleryState(
        request: GalleryRequest,
        deviceAuth: DeviceAuthState,
    ): GalleryLoadSnapshot {
        val firstPage = repository.storeIndexImagePage(
            connectionInput = deviceAuth.connectionBootstrapJson(),
            prefix = null,
            depth = GALLERY_FLATTENED_DEPTH,
            offset = 0,
            limit = request.pageSize,
            sort = resolveGalleryStoreSortOrder(request.sort),
            serverCaPem = deviceAuth.serverCaPem?.takeIf { it.isNotBlank() },
            clientIdentityJson = deviceAuth.toClientIdentityJson(),
        )
        val items = firstPage.entries.mapNotNull(::galleryImageItemFromEntry)
        val totalItemCount = firstPage.total_entry_count.coerceAtLeast(items.size)
        return GalleryLoadSnapshot(
            mode = GalleryViewMode.FLATTENED_ALL_IMAGES,
            currentDirectoryDocumentId = GALLERY_ROOT_DOCUMENT_ID,
            currentDirectoryPath = GALLERY_ROOT_PATH,
            breadcrumbs = emptyList(),
            directories = emptyList(),
            collection = GalleryCollectionState(
                totalItemCount = totalItemCount,
                pageSize = request.pageSize,
                pageCount = resolveGalleryPageCount(totalItemCount, request.pageSize),
            ),
            firstPageItems = items,
        )
    }

    private suspend fun loadCurrentDirectoryGalleryState(
        request: GalleryRequest,
        deviceAuth: DeviceAuthState,
    ): GalleryLoadSnapshot {
        val connectionInput = deviceAuth.connectionBootstrapJson()
        val serverCaPem = deviceAuth.serverCaPem?.takeIf { it.isNotBlank() }
        val clientIdentityJson = deviceAuth.toClientIdentityJson()
        val prefix = galleryPrefixForPath(request.currentDirectoryPath)
        val listing = repository.storeIndexDirectoryListing(
            connectionInput = connectionInput,
            prefix = prefix,
            depth = 1,
            serverCaPem = serverCaPem,
            clientIdentityJson = clientIdentityJson,
        )
        val firstPage = repository.storeIndexImagePage(
            connectionInput = connectionInput,
            prefix = prefix,
            depth = 1,
            offset = 0,
            limit = request.pageSize,
            sort = resolveGalleryStoreSortOrder(request.sort),
            serverCaPem = serverCaPem,
            clientIdentityJson = clientIdentityJson,
        )
        val directories = listing.entries.mapNotNull(::galleryDirectoryItemFromEntry)
        val items = firstPage.entries.mapNotNull(::galleryImageItemFromEntry)
        val totalItemCount = firstPage.total_entry_count.coerceAtLeast(items.size)

        return GalleryLoadSnapshot(
            mode = GalleryViewMode.CURRENT_DIRECTORY,
            currentDirectoryDocumentId = request.currentDirectoryDocumentId,
            currentDirectoryPath = request.currentDirectoryPath,
            breadcrumbs = request.breadcrumbs,
            directories = directories,
            collection = GalleryCollectionState(
                totalItemCount = totalItemCount,
                pageSize = request.pageSize,
                pageCount = resolveGalleryPageCount(totalItemCount, request.pageSize),
            ),
            firstPageItems = items,
        )
    }

    private suspend fun loadGalleryPage(
        request: GalleryRequest,
        pageIndex: Int,
        deviceAuth: DeviceAuthState,
    ): StoreIndexResponse {
        val pageSize = request.pageSize.coerceAtLeast(1)
        val offset = pageIndex.coerceAtLeast(0) * pageSize
        return repository.storeIndexImagePage(
            connectionInput = deviceAuth.connectionBootstrapJson(),
            prefix = when (request.mode) {
                GalleryViewMode.FLATTENED_ALL_IMAGES -> null
                GalleryViewMode.CURRENT_DIRECTORY -> galleryPrefixForPath(request.currentDirectoryPath)
            },
            depth = when (request.mode) {
                GalleryViewMode.FLATTENED_ALL_IMAGES -> GALLERY_FLATTENED_DEPTH
                GalleryViewMode.CURRENT_DIRECTORY -> 1
            },
            offset = offset,
            limit = pageSize,
            sort = resolveGalleryStoreSortOrder(request.sort),
            serverCaPem = deviceAuth.serverCaPem?.takeIf { it.isNotBlank() },
            clientIdentityJson = deviceAuth.toClientIdentityJson(),
        )
    }

    internal suspend fun refreshPersistedDeviceAuthState(): DeviceAuthState {
        val persisted = withContext(Dispatchers.IO) {
            IronmeshPreferences.getDeviceAuthState(getApplication())
        }
        if (persisted != uiState.value.deviceAuthState) {
            uiState.value = uiState.value.copy(deviceAuthState = persisted)
        }
        return persisted
    }

    private fun runTimingDiagnosticsOperation(
        actionDescription: String,
        markInProgress: (MainUiState) -> MainUiState,
        request: (TimingDiagnosticsCredentials) -> ConnectionRouteSnapshot,
        onSuccess: (MainUiState, ConnectionRouteSnapshot) -> MainUiState,
    ) {
        stopConnectionRoutesMonitor()
        uiState.value = markInProgress(uiState.value)
        viewModelScope.launch {
            val deviceAuth = runCatching { refreshPersistedDeviceAuthState() }
                .getOrElse { error ->
                    uiState.value = uiState.value.copy(
                        connectionRoutesError = error.message,
                        status = "Device identity unavailable: ${error.message}",
                    )
                    finishTimingDiagnosticsOperation()
                    return@launch
                }
            val credentials = timingDiagnosticsCredentialsOrNull(
                actionDescription = actionDescription,
                deviceAuth = deviceAuth,
            ) ?: run {
                finishTimingDiagnosticsOperation()
                return@launch
            }
            runCatching {
                withContext(Dispatchers.IO) {
                    request(credentials)
                }
            }
                .onSuccess { snapshot ->
                    uiState.value = onSuccess(uiState.value, snapshot)
                }
                .onFailure { error ->
                    val detail = error.message ?: "unknown error"
                    uiState.value = uiState.value.copy(
                        connectionRoutesError = detail,
                        status = "Failed to $actionDescription: $detail",
                    )
                }
            finishTimingDiagnosticsOperation()
        }
    }

    private fun timingDiagnosticsCredentialsOrNull(
        actionDescription: String,
        deviceAuth: DeviceAuthState,
    ): TimingDiagnosticsCredentials? {
        val connectionInput = deviceAuth.connectionBootstrapJson()
        val clientIdentityJson = deviceAuth.toClientIdentityJson()
        if (connectionInput.isBlank() || clientIdentityJson.isNullOrBlank()) {
            val message = "Enroll this device to $actionDescription."
            uiState.value = uiState.value.copy(
                connectionRoutesError = message,
                status = message,
            )
            return null
        }
        return TimingDiagnosticsCredentials(
            connectionInput = connectionInput,
            serverCaPem = deviceAuth.serverCaPem?.takeIf { it.isNotBlank() },
            clientIdentityJson = clientIdentityJson,
        )
    }

    private fun finishTimingDiagnosticsOperation() {
        uiState.value = uiState.value.copy(
            timingMeasurementResetting = false,
            timingStoreIndexTestRunning = false,
        )
        resumeConnectionRoutesMonitorIfNeeded()
    }

    private fun resumeConnectionRoutesMonitorIfNeeded() {
        if (
            uiObservationGate.observationJobsActive &&
            uiState.value.selectedSection.isConnectionDiagnosticsSection()
        ) {
            startConnectionRoutesMonitor()
        }
    }

    private fun configureTitleLatencyMonitor() {
        titleLatencyConfigurationJob?.cancel()
        titleLatencyConfigurationJob = null
        stopTitleLatencyStatusMonitor()

        val settings = uiState.value.titleLatencyMonitorSettings
        titleLatencyConfigurationJob = viewModelScope.launch {
            val deviceAuth = runCatching { refreshPersistedDeviceAuthState() }
                .getOrElse { error ->
                    uiState.value = uiState.value.withTitleLatencyPollResult(
                        TitleLatencyProbeStatus(
                            state = "failed",
                            error = error.message ?: "Device identity is unavailable",
                        ),
                    )
                    titleLatencyConfigurationJob = null
                    return@launch
                }
            val connectionInput = deviceAuth.connectionBootstrapJson()
            val clientIdentityJson = deviceAuth.toClientIdentityJson()
            if (connectionInput.isBlank() || clientIdentityJson.isNullOrBlank()) {
                uiState.value = uiState.value.withTitleLatencyPollResult(TitleLatencyProbeStatus())
                titleLatencyConfigurationJob = null
                return@launch
            }
            val configured = runCatching {
                withContext(Dispatchers.IO) {
                    repository.configureTitleLatencyMonitor(
                        connectionInput = connectionInput,
                        serverCaPem = deviceAuth.serverCaPem?.takeIf { it.isNotBlank() },
                        clientIdentityJson = clientIdentityJson,
                        settings = settings,
                    )
                }
            }
            if (!isActive) {
                return@launch
            }

            val initialStatus = configured.getOrElse { error ->
                TitleLatencyProbeStatus(
                    state = "failed",
                    error = error.message ?: "Failed to configure the title latency monitor",
                )
            }
            uiState.value = uiState.value.withTitleLatencyPollResult(initialStatus)
            titleLatencyConfigurationJob = null
            if (configured.isSuccess && settings.enabled) {
                startTitleLatencyStatusMonitor()
            }
        }
    }

    private fun startTitleLatencyStatusMonitor() {
        if (
            !uiObservationGate.observationJobsActive ||
            !uiState.value.titleLatencyMonitorSettings.enabled ||
            titleLatencyConfigurationJob?.isActive == true ||
            titleLatencyStatusMonitorJob?.isActive == true
        ) {
            return
        }
        titleLatencyStatusMonitorJob = viewModelScope.launch {
            while (isActive) {
                delay(TITLE_LATENCY_STATUS_POLL_MS)
                val nextStatus = withContext(Dispatchers.IO) {
                    runCatching { repository.getTitleLatencyStatus() }.getOrNull()
                } ?: continue
                uiState.value = uiState.value.withTitleLatencyPollResult(nextStatus)
            }
        }
    }

    private fun stopTitleLatencyStatusMonitor() {
        titleLatencyStatusMonitorJob?.cancel()
        titleLatencyStatusMonitorJob = null
    }

    private fun startConnectionRoutesMonitor() {
        if (
            !uiObservationGate.observationJobsActive ||
            connectionRoutesMonitorJob?.isActive == true
        ) {
            return
        }
        connectionRoutesMonitorJob = viewModelScope.launch {
            loadConnectionRoutes(
                refresh = false,
                showLoading = uiState.value.connectionRoutes == null,
                statusOnFailure = false,
            )
            while (isActive) {
                delay(CONNECTION_ROUTE_SNAPSHOT_POLL_MS)
                if (uiState.value.connectionRoutesLoading) {
                    continue
                }
                loadConnectionRoutes(
                    refresh = false,
                    showLoading = false,
                    statusOnFailure = false,
                )
            }
        }
    }

    private fun stopConnectionRoutesMonitor() {
        connectionRoutesMonitorJob?.cancel()
        connectionRoutesMonitorJob = null
    }

    private fun startEnrollmentVerificationMonitor(
        authState: DeviceAuthState,
        startedAtNanos: Long,
    ) {
        stopEnrollmentVerificationMonitor()
        enrollmentVerificationMonitorJob = viewModelScope.launch {
            while (isActive) {
                delay(ENROLLMENT_VERIFICATION_POLL_MS)
                val routes = withContext(Dispatchers.IO) {
                    runCatching {
                        repository.getConnectionRouteSnapshot(
                            connectionInput = authState.connectionBootstrapJson(),
                            serverCaPem = authState.serverCaPem?.takeIf { it.isNotBlank() },
                            clientIdentityJson = authState.toClientIdentityJson(),
                            refresh = false,
                        )
                    }.getOrNull()
                }
                val detail = enrollmentVerificationProgressDetail(
                    elapsedMs = elapsedMillisSince(startedAtNanos),
                    connectionRoutes = routes,
                )
                val diagnostic = uiState.value.enrollmentDiagnostics
                    .firstOrNull { it.id == EnrollmentDiagnosticStepId.VERIFY_ACCESS }
                if (diagnostic?.status != EnrollmentDiagnosticStepStatus.IN_PROGRESS) {
                    return@launch
                }
                uiState.value = uiState.value.copy(
                    enrollmentDiagnostics = uiState.value.enrollmentDiagnostics
                        .withEnrollmentDiagnosticStatus(
                            stepId = EnrollmentDiagnosticStepId.VERIFY_ACCESS,
                            status = EnrollmentDiagnosticStepStatus.IN_PROGRESS,
                            detail = detail,
                        ),
                )
                Log.i(ENROLLMENT_LOG_TAG, detail)
            }
        }
    }

    private fun stopEnrollmentVerificationMonitor() {
        enrollmentVerificationMonitorJob?.cancel()
        enrollmentVerificationMonitorJob = null
    }

    private fun elapsedMillisSince(startedAtNanos: Long): Long {
        return ((System.nanoTime() - startedAtNanos) / 1_000_000L).coerceAtLeast(0L)
    }

    private suspend fun loadConnectionRoutes(
        refresh: Boolean,
        showLoading: Boolean,
        statusOnFailure: Boolean,
    ) {
        val deviceAuth = try {
            refreshPersistedDeviceAuthState()
        } catch (error: DeviceIdentityStorageException) {
            uiState.value = uiState.value.copy(
                connectionRoutesLoading = false,
                connectionRoutesError = error.message,
                status = if (statusOnFailure) {
                    "Device identity unavailable: ${error.message}"
                } else {
                    uiState.value.status
                },
            )
            return
        }
        val connectionInput = deviceAuth.connectionBootstrapJson()
        val clientIdentityJson = deviceAuth.toClientIdentityJson()
        if (connectionInput.isBlank() || clientIdentityJson.isNullOrBlank()) {
            uiState.value = uiState.value.copy(
                connectionRoutesLoading = false,
                connectionRoutesError = "Enroll this device to inspect connection paths.",
            )
            return
        }

        if (showLoading) {
            uiState.value = uiState.value.copy(
                connectionRoutesLoading = true,
                connectionRoutesError = null,
            )
        }

        runCatching {
            withContext(Dispatchers.IO) {
                repository.getConnectionRouteSnapshot(
                    connectionInput = connectionInput,
                    serverCaPem = deviceAuth.serverCaPem?.takeIf { it.isNotBlank() },
                    clientIdentityJson = clientIdentityJson,
                    refresh = refresh,
                )
            }
        }
            .onSuccess { snapshot ->
                uiState.value = uiState.value.withConnectionRoutePollResult(
                    result = snapshot,
                    loadedAtUnixMs = System.currentTimeMillis(),
                )
            }
            .onFailure { error ->
                uiState.value = uiState.value.copy(
                    connectionRoutesLoading = false,
                    connectionRoutesError = error.message ?: "Failed to load connection paths",
                    status = if (statusOnFailure) {
                        "Error: ${error.message}"
                    } else {
                        uiState.value.status
                    },
                )
            }
    }

    private fun galleryImageItemFromEntry(entry: StoreIndexEntry): GalleryImageItem? {
        if (entry.entry_type != "key") {
            return null
        }
        val path = entry.path.trim().trim('/')
        if (path.isBlank()) {
            return null
        }
        val application = getApplication<Application>()
        val authority = "${application.packageName}.documents"
        return GalleryImageItem(
            documentUri = DocumentsContract.buildDocumentUri(authority, galleryFileDocumentId(path)),
            displayName = path.substringAfterLast('/'),
            remotePath = path,
            mimeType = entry.media?.mime_type ?: "image/*",
            createdAtUnixMs = entry.media?.taken_at_unix?.times(1000),
            width = entry.media?.width,
            height = entry.media?.height,
            thumbnailStatus = entry.media?.status,
        )
    }

    private fun galleryDirectoryItemFromEntry(entry: StoreIndexEntry): GalleryDirectoryItem? {
        if (entry.entry_type != "prefix") {
            return null
        }
        val normalizedPath = entry.path.trim().trim('/').removeSuffix("/")
        if (normalizedPath.isBlank()) {
            return null
        }
        return GalleryDirectoryItem(
            documentId = galleryDirectoryDocumentId(normalizedPath),
            displayName = normalizedPath.substringAfterLast('/'),
            pathLabel = normalizeGalleryDirectoryPath(normalizedPath),
        )
    }

    private fun resolveGalleryStoreSortOrder(sort: GallerySortOption): StoreIndexSortOrder {
        return when (sort) {
            GallerySortOption.CREATION_TIME -> StoreIndexSortOrder.CAPTURED_DESC
            GallerySortOption.NAME -> StoreIndexSortOrder.PATH_ASC
        }
    }

    private fun resolveGalleryPageCount(
        totalItemCount: Int,
        pageSize: Int,
    ): Int {
        if (totalItemCount <= 0) {
            return 0
        }
        return (totalItemCount + pageSize.coerceAtLeast(1) - 1) / pageSize.coerceAtLeast(1)
    }

    private fun galleryPrefixForPath(path: String): String? {
        val normalized = path.trim().trim('/')
        return normalized.takeIf { it.isNotBlank() }
    }

    private fun galleryDirectoryDocumentId(path: String): String {
        return if (path.isBlank()) GALLERY_ROOT_DOCUMENT_ID else "dir:${path.trim('/')}"
    }

    private fun galleryFileDocumentId(path: String): String = "file:${path.trim('/')}"

    private fun pageIndexForGalleryEntry(
        index: Int,
        pageSize: Int,
    ): Int {
        return index / pageSize.coerceAtLeast(1)
    }

    private fun galleryEntryWithinPage(
        index: Int,
        pageSize: Int,
    ): Int {
        return index % pageSize.coerceAtLeast(1)
    }

    private fun nextGalleryRequestVersion(): Int {
        galleryRequestVersion += 1
        return galleryRequestVersion
    }

    private fun isCurrentGalleryRequest(requestVersion: Int): Boolean {
        return requestVersion == galleryRequestVersion
    }

    private fun normalizeGalleryDirectoryPath(path: String): String {
        val normalized = path.trim().trim('/')
        return if (normalized.isBlank()) GALLERY_ROOT_PATH else "$normalized/"
    }
}

internal fun MainUiState.withGalleryRefreshStarted(): MainUiState {
    return copy(
        galleryLoading = true,
        galleryError = null,
        galleryCollection = null,
        galleryPages = emptyMap(),
        status = "Loading gallery...",
    )
}

internal fun MainUiState.withGalleryRefreshFailure(error: Throwable): MainUiState {
    val galleryError = galleryLoadErrorFrom(error)
    return copy(
        galleryLoading = false,
        galleryError = galleryError,
        status = galleryLoadFailureStatus(galleryError),
    )
}

internal fun galleryLoadErrorFrom(error: Throwable): GalleryLoadError {
    val causes = generateSequence(error) { current -> current.cause }
        .take(8)
        .toList()
    val technicalDetail = causes
        .mapNotNull { cause -> cause.message?.trim()?.takeIf(String::isNotEmpty) }
        .lastOrNull()
        ?: causes.lastOrNull()?.javaClass?.simpleName?.takeIf(String::isNotEmpty)
    val timedOut = causes.any { cause ->
        cause.javaClass.simpleName.contains("timeout", ignoreCase = true) ||
            cause.message?.let { message ->
                message.contains("timeout", ignoreCase = true) ||
                    message.contains("timed out", ignoreCase = true)
            } == true
    }
    return GalleryLoadError(
        kind = if (timedOut) GalleryLoadErrorKind.TIMEOUT else GalleryLoadErrorKind.REQUEST_FAILED,
        technicalDetail = technicalDetail,
    )
}

private fun galleryLoadFailureStatus(error: GalleryLoadError): String {
    val summary = when (error.kind) {
        GalleryLoadErrorKind.TIMEOUT -> "Gallery index request timed out"
        GalleryLoadErrorKind.REQUEST_FAILED -> "Gallery index request failed"
    }
    return error.technicalDetail
        ?.takeIf { it.isNotBlank() }
        ?.let { detail -> "$summary: $detail" }
        ?: summary
}

private fun MainSection.isConnectionDiagnosticsSection(): Boolean {
    return this == MainSection.CONNECTIVITY || this == MainSection.REQUEST_TIMINGS
}
