package io.ironmesh.android

import android.content.ActivityNotFoundException
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.provider.DocumentsContract
import android.webkit.WebView
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.lifecycle.viewmodel.compose.viewModel
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import io.ironmesh.android.data.FolderSyncNetworkPolicy
import io.ironmesh.android.data.AndroidDiagnosticLog as Log
import io.ironmesh.android.data.EmbeddedWebUiSession
import io.ironmesh.android.work.FolderSyncForegroundService
import io.ironmesh.android.ui.MainSection
import io.ironmesh.android.ui.MainViewModel
import io.ironmesh.android.ui.LibraryScreenActions
import io.ironmesh.android.ui.SyncScreenActions
import io.ironmesh.android.ui.components.IronmeshAppShell
import io.ironmesh.android.ui.components.requestGalleryMapFullscreenExit
import io.ironmesh.android.ui.screens.ConnectionPathsScreen
import io.ironmesh.android.ui.screens.HomeScreen
import io.ironmesh.android.ui.screens.GalleryMapScreen
import io.ironmesh.android.ui.screens.LibraryScreen
import io.ironmesh.android.ui.screens.OnboardingScreen
import io.ironmesh.android.ui.screens.RequestTimingsScreen
import io.ironmesh.android.ui.screens.SettingsScreen
import io.ironmesh.android.ui.screens.SyncScreen
import io.ironmesh.android.ui.theme.IronmeshTheme
import io.ironmesh.android.ui.toConnectivityUiState
import io.ironmesh.android.ui.toGalleryMapUiState
import io.ironmesh.android.ui.toHomeUiState
import io.ironmesh.android.ui.toLibraryUiState
import io.ironmesh.android.ui.toOnboardingUiState
import io.ironmesh.android.ui.toRequestTimingsUiState
import io.ironmesh.android.ui.toSettingsUiState
import io.ironmesh.android.ui.toSyncUiState
import java.time.Instant
import java.time.ZoneOffset
import java.time.format.DateTimeFormatter

class MainActivity : ComponentActivity() {
    private var systemAccessState by mutableStateOf(AndroidSystemAccessState())

    override fun onResume() {
        super.onResume()
        refreshSystemAccessState()
        FolderSyncForegroundService.appForegrounded(this)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        refreshSystemAccessState()

        setContent {
            val vm: MainViewModel = viewModel()
            val state by vm.uiState
            var galleryMapFullscreen by remember { mutableStateOf(false) }
            var galleryMapWebView by remember { mutableStateOf<WebView?>(null) }
            val showsGalleryMapFullscreen =
                state.selectedSection == MainSection.GALLERY_MAP && galleryMapFullscreen

            LaunchedEffect(state.selectedSection) {
                if (state.selectedSection != MainSection.GALLERY_MAP) {
                    galleryMapFullscreen = false
                    galleryMapWebView = null
                }
            }
            LaunchedEffect(showsGalleryMapFullscreen) {
                WindowInsetsControllerCompat(window, window.decorView).apply {
                    systemBarsBehavior =
                        WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
                    if (showsGalleryMapFullscreen) {
                        hide(WindowInsetsCompat.Type.statusBars())
                    } else {
                        show(WindowInsetsCompat.Type.statusBars())
                    }
                }
            }
            BackHandler(enabled = showsGalleryMapFullscreen) {
                val webView = galleryMapWebView
                if (webView == null) {
                    galleryMapFullscreen = false
                } else {
                    webView.requestGalleryMapFullscreenExit()
                }
            }
            val syncActions = remember(vm) {
                SyncScreenActions(
                    runNow = vm::runFolderSyncNow,
                    updateNewProfileLabel = vm::updateNewSyncLabel,
                    updateNewProfilePrefix = vm::updateNewSyncPrefix,
                    updateNewProfileLocalFolder = vm::updateNewSyncLocalFolder,
                    updateNewProfileAllowWifi = vm::updateNewSyncAllowWifi,
                    updateNewProfileAllowCellular = vm::updateNewSyncAllowCellular,
                    updateNewProfileAllowOtherConnections = vm::updateNewSyncAllowOtherConnections,
                    updateNewProfileAllowRoaming = vm::updateNewSyncAllowRoaming,
                    updateNewProfileAllowedWifiSsids = vm::updateNewSyncAllowedWifiSsids,
                    addProfile = vm::addFolderSyncProfile,
                    toggleHistory = vm::toggleFolderSyncHistory,
                    setProfileEnabled = vm::setFolderSyncProfileEnabled,
                    removeProfile = vm::removeFolderSyncProfile,
                    setHistoryFilter = vm::setFolderSyncHistoryFilter,
                    loadMoreHistory = vm::loadMoreFolderSyncHistory,
                    updateProfileNetworkPolicy = vm::updateFolderSyncProfileNetworkPolicy,
                )
            }
            val libraryActions = remember(vm) {
                LibraryScreenActions(
                    updateVisiblePages = vm::updateVisibleGalleryPages,
                    refresh = vm::refreshGallery,
                    updateViewMode = vm::updateGalleryViewMode,
                    updateSort = vm::updateGallerySort,
                    updateShowSensitiveContent = vm::updateGalleryShowSensitiveContent,
                    navigateToRoot = vm::navigateGalleryToRoot,
                    navigateUp = vm::navigateGalleryUp,
                    navigateToBreadcrumb = vm::navigateGalleryToBreadcrumb,
                    openDirectory = vm::openGalleryDirectory,
                    retryPage = vm::retryGalleryPage,
                    itemAt = vm::galleryItemAt,
                    ensureItemLoaded = vm::ensureGalleryItemLoaded,
                    pinItem = vm::pinGalleryItem,
                    toggleMediaLabel = vm::toggleGalleryMediaLabel,
                )
            }
            IronmeshTheme(accentColorHex = state.themeAccentColorHex) {
                val context = LocalContext.current
                val snackbarHostState = remember { SnackbarHostState() }
                var lastSnackbarMessage by rememberSaveable { mutableStateOf("") }
                var openWebUiWhenReady by rememberSaveable { mutableStateOf(false) }
                var existingProfilePermissionRequestAttempted by rememberSaveable { mutableStateOf(false) }

                val scanLauncher = rememberLauncherForActivityResult(ScanContract()) { result ->
                    if (result.contents != null) {
                        vm.updateBootstrapInput(result.contents)
                    }
                }
                val photoAccessPermissionLauncher = rememberLauncherForActivityResult(
                    ActivityResultContracts.RequestMultiplePermissions(),
                ) { grants ->
                    val denied = grants
                        .filterValues { granted -> !granted }
                        .keys
                        .toList()
                    if (denied.isEmpty()) {
                        Log.i("MainActivity", "Granted photo original-byte permissions for folder sync")
                        vm.setStatus("Photo metadata access granted for folder sync")
                    } else {
                        Log.i(
                            "MainActivity",
                            "Photo original-byte permissions denied: ${denied.joinToString(", ")}",
                        )
                        vm.setStatus("Photo GPS EXIF may be stripped until photo permissions are granted")
                    }
                    refreshSystemAccessState()
                }
                val wifiNamePermissionLauncher = rememberLauncherForActivityResult(
                    ActivityResultContracts.RequestMultiplePermissions(),
                ) { grants ->
                    val denied = grants
                        .filterValues { granted -> !granted }
                        .keys
                        .toList()
                    if (denied.isEmpty()) {
                        vm.setStatus("Wi-Fi name access granted for restricted sync rules")
                    } else {
                        vm.setStatus(
                            "Allowed Wi-Fi names need Android Wi-Fi/location access before they can be enforced",
                        )
                    }
                    refreshSystemAccessState()
                }
                val folderPickerLauncher = rememberLauncherForActivityResult(
                    ActivityResultContracts.StartActivityForResult(),
                ) { result ->
                    handleFolderPickerResult(
                        context = context,
                        result = result,
                        onResolvedSelection = { path, treeUri ->
                            vm.updateNewSyncLocalFolderSelection(path, treeUri)
                            vm.setStatus("Selected sync folder: $path")
                            requestOriginalPhotoAccessIfNeeded(context, photoAccessPermissionLauncher)
                        },
                        onError = vm::setStatus,
                    )
                }
                val diagnosticLogLauncher = rememberLauncherForActivityResult(
                    ActivityResultContracts.CreateDocument("text/plain"),
                ) { destination ->
                    destination?.let(vm::exportDiagnosticLog)
                }

                val onScanQr: () -> Unit = {
                    scanLauncher.launch(
                        ScanOptions().apply {
                            setPrompt("Scan bootstrap claim QR code")
                            setBeepEnabled(false)
                            setOrientationLocked(false)
                        },
                    )
                }
                val onPickLocalFolder: () -> Unit = {
                    launchFolderPicker(
                        launcher = folderPickerLauncher,
                        onError = vm::setStatus,
                    )
                }
                val onEnsureWifiNameAccess: (FolderSyncNetworkPolicy) -> Unit = { policy ->
                    requestWifiNameAccessIfNeeded(
                        context = context,
                        launcher = wifiNamePermissionLauncher,
                        policy = policy,
                    )
                }
                val onOpenWebConsole: () -> Unit = remember(vm) {
                    {
                        val currentState = vm.uiState.value
                        if (!currentState.loading) {
                            currentState.webUiSession?.let { session ->
                                openWebUi(session, vm::setStatus)
                            } ?: run {
                                openWebUiWhenReady = true
                                vm.startWebUi()
                            }
                        }
                    }
                }
                LaunchedEffect(openWebUiWhenReady, state.loading, state.webUiSession) {
                    if (!openWebUiWhenReady || state.loading) {
                        return@LaunchedEffect
                    }
                    state.webUiSession?.let { session ->
                        openWebUi(session, vm::setStatus)
                    }
                    openWebUiWhenReady = false
                }

                LaunchedEffect(state.syncProfiles) {
                    if (existingProfilePermissionRequestAttempted || state.syncProfiles.isEmpty()) {
                        return@LaunchedEffect
                    }

                    existingProfilePermissionRequestAttempted = true
                    requestOriginalPhotoAccessIfNeeded(context, photoAccessPermissionLauncher)
                }

                LaunchedEffect(state.status) {
                    if (
                        state.status.isBlank() ||
                        state.status == "Ready" ||
                        state.status == lastSnackbarMessage
                    ) {
                        return@LaunchedEffect
                    }
                    lastSnackbarMessage = state.status
                    snackbarHostState.showSnackbar(state.status)
                }

                if (!state.persistedStateLoaded) {
                    Scaffold(
                        snackbarHost = { SnackbarHost(snackbarHostState) },
                    ) { contentPadding ->
                        Box(
                            modifier = Modifier
                                .fillMaxSize()
                                .padding(contentPadding),
                            contentAlignment = Alignment.Center,
                        ) {
                            CircularProgressIndicator()
                        }
                    }
                } else if (!state.deviceIdentity.isEnrolled) {
                    Scaffold(
                        snackbarHost = { SnackbarHost(snackbarHostState) },
                    ) { _ ->
                        OnboardingScreen(
                            state = state.toOnboardingUiState(),
                            onDeviceLabelChange = vm::updateDeviceLabelInput,
                            onBootstrapInputChange = vm::updateBootstrapInput,
                            onScanQr = onScanQr,
                            onEnroll = vm::enrollDevice,
                        )
                    }
                } else {
                    IronmeshAppShell(
                        selectedSection = state.selectedSection,
                        onSelectSection = vm::selectSection,
                        snackbarHostState = snackbarHostState,
                        deviceLabel = state.deviceIdentity.label,
                        titleLatencyStatus = state.titleLatencyStatus,
                        onOpenConnectionDiagnostics = {
                            vm.selectSection(MainSection.CONNECTIVITY)
                        },
                        onExportDiagnosticLog = {
                            runCatching {
                                diagnosticLogLauncher.launch(diagnosticLogFileName())
                            }.onFailure { error ->
                                vm.setStatus("Unable to open the diagnostic log save dialog: ${error.message}")
                            }
                        },
                        onNavigateBack = if (
                            state.selectedSection == MainSection.CONNECTIVITY ||
                            state.selectedSection == MainSection.REQUEST_TIMINGS
                        ) {
                            { vm.selectSection(MainSection.SETTINGS) }
                        } else {
                            null
                        },
                        fullscreenContent = showsGalleryMapFullscreen,
                        topBarActions = {
                            if (
                                state.selectedSection == MainSection.CONNECTIVITY ||
                                state.selectedSection == MainSection.REQUEST_TIMINGS
                            ) {
                                TextButton(
                                    onClick = vm::refreshConnectionRoutes,
                                    enabled = !state.connectionRoutesLoading &&
                                        !state.timingMeasurementResetting &&
                                        !state.timingStoreIndexTestRunning,
                                ) {
                                    Text(
                                        if (state.connectionRoutesLoading) {
                                            getString(R.string.connection_paths_refreshing)
                                        } else {
                                            getString(R.string.connection_paths_refresh)
                                        },
                                    )
                                }
                            }
                        },
                    ) { contentModifier ->
                        Box(modifier = contentModifier) {
                            when (state.selectedSection) {
                                MainSection.HOME -> HomeScreen(
                                    state = state.toHomeUiState(),
                                    onRunSyncNow = vm::runFolderSyncNow,
                                    onRetryConnection = vm::retryAppConnection,
                                    onOpenWebConsole = onOpenWebConsole,
                                    onOpenSync = { vm.selectSection(MainSection.SYNC) },
                                    onSelectSection = vm::selectSection,
                                )

                                MainSection.CONNECTIVITY -> ConnectionPathsScreen(
                                    state = state.toConnectivityUiState(),
                                    onRefresh = vm::refreshConnectionRoutes,
                                )

                                MainSection.REQUEST_TIMINGS -> RequestTimingsScreen(
                                    state = state.toRequestTimingsUiState(),
                                    onRefresh = vm::refreshConnectionRoutes,
                                    onResetMeasurement = vm::resetTimingMeasurement,
                                    onRunStoreIndexTest = vm::runTimingStoreIndexTest,
                                )

                                MainSection.SYNC -> SyncScreen(
                                    state = state.toSyncUiState(),
                                    actions = syncActions,
                                    onPickLocalFolder = onPickLocalFolder,
                                    onEnsureWifiNameAccess = onEnsureWifiNameAccess,
                                )

                                MainSection.LIBRARY -> LibraryScreen(
                                    state = state.toLibraryUiState(),
                                    actions = libraryActions,
                                )

                                MainSection.GALLERY_MAP -> GalleryMapScreen(
                                    state = state.toGalleryMapUiState(),
                                    onStartGalleryMap = vm::startWebUi,
                                    onFullscreenChanged = { fullscreen ->
                                        galleryMapFullscreen = fullscreen
                                    },
                                    onWebViewCreated = { webView ->
                                        galleryMapWebView = webView
                                    },
                                )

                                MainSection.SETTINGS -> SettingsScreen(
                                    state = state.toSettingsUiState(),
                                    hasPhotoAccess = systemAccessState.hasPhotoAccess,
                                    hasWifiNamePermissions = systemAccessState.hasWifiNamePermissions,
                                    isLocationEnabled = systemAccessState.isLocationEnabled,
                                    onRequestPhotoAccess = {
                                        requestOriginalPhotoAccessIfNeeded(
                                            context,
                                            photoAccessPermissionLauncher,
                                        )
                                    },
                                    onRequestWifiNameAccess = {
                                        val missing = missingWifiNameAccessPermissions(context)
                                        if (missing.isNotEmpty()) {
                                            wifiNamePermissionLauncher.launch(missing)
                                        }
                                    },
                                    onOpenLocationSettings = {
                                        if (!openLocationSettings(context)) {
                                            vm.setStatus("Unable to open Android location settings")
                                        }
                                    },
                                    onOpenFiles = { openFilesAtIronmeshRoot(vm) },
                                    onOpenConnectionDiagnostics = {
                                        vm.selectSection(MainSection.CONNECTIVITY)
                                    },
                                    onOpenTimingDiagnostics = {
                                        vm.selectSection(MainSection.REQUEST_TIMINGS)
                                    },
                                    onOpenWebConsole = onOpenWebConsole,
                                    onClearCachedData = vm::clearCachedData,
                                    onThemeAccentColorChange = vm::updateThemeAccentColor,
                                    onTitleLatencyMonitorEnabledChange = vm::updateTitleLatencyMonitorEnabled,
                                    onTitleLatencyMonitorPeriodSecondsChange =
                                        vm::updateTitleLatencyMonitorPeriodSeconds,
                                    onRevealNodePriorities = vm::refreshConnectionRoutes,
                                    onNodePriorityOverrideChange =
                                        vm::updateNodeConnectionPriorityOverride,
                                    onKeyChange = vm::updateKey,
                                    onPayloadChange = vm::updatePayload,
                                    onPutObject = vm::putObject,
                                    onGetObject = vm::getObject,
                                )
                            }
                        }
                    }
                }
            }
        }
    }

    private fun refreshSystemAccessState() {
        systemAccessState = readAndroidSystemAccessState(this)
    }

    private fun openWebUi(
        session: EmbeddedWebUiSession,
        onStatus: (String) -> Unit,
    ) {
        if (session.url.isBlank() || session.authorization.isBlank()) {
            onStatus("Web UI is not ready yet")
            return
        }

        startActivity(WebUiActivity.intent(this, session))
        onStatus("Opened embedded Web UI")
    }

    private fun openFilesAtIronmeshRoot(vm: MainViewModel) {
        val authority = "${packageName}.documents"
        val rootTreeUri = DocumentsContract.buildTreeDocumentUri(authority, "dir:")

        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).apply {
            addFlags(
                Intent.FLAG_GRANT_READ_URI_PERMISSION or
                    Intent.FLAG_GRANT_WRITE_URI_PERMISSION or
                    Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION or
                    Intent.FLAG_GRANT_PREFIX_URI_PERMISSION,
            )

            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                putExtra(DocumentsContract.EXTRA_INITIAL_URI, rootTreeUri)
            }
        }

        val preferredPackages = listOf(
            "com.google.android.documentsui",
            "com.android.documentsui",
            "com.google.android.apps.nbu.files",
        )

        val launchIntent = preferredPackages
            .asSequence()
            .map { pkg -> Intent(intent).setPackage(pkg) }
            .firstOrNull { candidate ->
                candidate.resolveActivity(packageManager) != null
            } ?: intent

        try {
            startActivity(launchIntent)
            vm.setStatus("Opened Files picker at BerryKeep root")
        } catch (_: ActivityNotFoundException) {
            vm.setStatus("No compatible Files app found on this device")
        }
    }
}

internal fun diagnosticLogFileName(now: Instant = Instant.now()): String {
    val timestamp = DateTimeFormatter
        .ofPattern("yyyyMMdd-HHmmss")
        .withZone(ZoneOffset.UTC)
        .format(now)
    return "berrykeep-diagnostic-$timestamp.log"
}
