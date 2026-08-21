package io.ironmesh.android.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.NavigationRail
import androidx.compose.material3.NavigationRailItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import io.ironmesh.android.R
import io.ironmesh.android.ui.MainSection
import io.ironmesh.android.data.TitleLatencyProbeStatus
import kotlin.math.roundToInt

@Composable
fun IronmeshAppShell(
    selectedSection: MainSection,
    onSelectSection: (MainSection) -> Unit,
    snackbarHostState: SnackbarHostState,
    deviceLabel: String?,
    titleLatencyStatus: TitleLatencyProbeStatus,
    onOpenConnectionDiagnostics: () -> Unit,
    onExportDiagnosticLog: () -> Unit,
    onNavigateBack: (() -> Unit)? = null,
    topBarActions: @Composable RowScope.() -> Unit = {},
    fullscreenContent: Boolean = false,
    content: @Composable (Modifier) -> Unit,
) {
    BoxWithConstraints(
        modifier = Modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.background),
    ) {
        val useRail = maxWidth >= 720.dp
        if (useRail) {
            Row(modifier = Modifier.fillMaxSize()) {
                if (!fullscreenContent) {
                    Surface(
                        modifier = Modifier.fillMaxHeight(),
                        color = MaterialTheme.colorScheme.surface,
                    ) {
                        Column(
                            modifier = Modifier
                                .statusBarsPadding()
                                .padding(top = 8.dp),
                        ) {
                            NavigationRail(
                                containerColor = MaterialTheme.colorScheme.surface,
                            ) {
                                shellItems().forEach { item ->
                                    NavigationRailItem(
                                        selected = selectedSection == item.section,
                                        onClick = { onSelectSection(item.section) },
                                        icon = {},
                                        label = { Text(stringResource(item.labelRes)) },
                                    )
                                }
                            }
                        }
                    }
                }
                Scaffold(
                    modifier = if (fullscreenContent) Modifier.fillMaxSize() else Modifier.weight(1f),
                    topBar = {
                        if (!fullscreenContent) {
                            IronmeshTopBar(
                                selectedSection = selectedSection,
                                deviceLabel = deviceLabel,
                                titleLatencyStatus = titleLatencyStatus,
                                onOpenConnectionDiagnostics = onOpenConnectionDiagnostics,
                                onExportDiagnosticLog = onExportDiagnosticLog,
                                onNavigateBack = onNavigateBack,
                                actions = topBarActions,
                            )
                        }
                    },
                    snackbarHost = {
                        if (!fullscreenContent) {
                            SnackbarHost(hostState = snackbarHostState)
                        }
                    },
                    contentWindowInsets = shellContentWindowInsets(fullscreenContent),
                ) { innerPadding ->
                    content(
                        Modifier
                            .fillMaxSize()
                            .padding(innerPadding)
                            .then(
                                if (fullscreenContent) {
                                    Modifier
                                } else {
                                    Modifier.padding(horizontal = 20.dp, vertical = 16.dp)
                                },
                            ),
                    )
                }
            }
        } else {
            Scaffold(
                topBar = {
                    if (!fullscreenContent) {
                        IronmeshTopBar(
                            selectedSection = selectedSection,
                            deviceLabel = deviceLabel,
                            titleLatencyStatus = titleLatencyStatus,
                            onOpenConnectionDiagnostics = onOpenConnectionDiagnostics,
                            onExportDiagnosticLog = onExportDiagnosticLog,
                            onNavigateBack = onNavigateBack,
                            actions = topBarActions,
                        )
                    }
                },
                bottomBar = {
                    if (!fullscreenContent) {
                        NavigationBar {
                            shellItems().forEach { item ->
                                NavigationBarItem(
                                    selected = selectedSection == item.section,
                                    onClick = { onSelectSection(item.section) },
                                    icon = {},
                                    label = { Text(stringResource(item.labelRes)) },
                                )
                            }
                        }
                    }
                },
                snackbarHost = {
                    if (!fullscreenContent) {
                        SnackbarHost(hostState = snackbarHostState)
                    }
                },
                contentWindowInsets = shellContentWindowInsets(fullscreenContent),
            ) { innerPadding ->
                content(
                    Modifier
                        .fillMaxSize()
                        .padding(innerPadding)
                        .then(
                            if (fullscreenContent) {
                                Modifier
                            } else {
                                Modifier.padding(horizontal = 16.dp, vertical = 12.dp)
                            },
                        ),
                )
            }
        }
    }
}

@Composable
private fun shellContentWindowInsets(fullscreenContent: Boolean): WindowInsets =
    if (fullscreenContent) {
        WindowInsets(0, 0, 0, 0)
    } else {
        WindowInsets.navigationBars
    }

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun IronmeshTopBar(
    selectedSection: MainSection,
    deviceLabel: String?,
    titleLatencyStatus: TitleLatencyProbeStatus,
    onOpenConnectionDiagnostics: () -> Unit,
    onExportDiagnosticLog: () -> Unit,
    onNavigateBack: (() -> Unit)?,
    actions: @Composable RowScope.() -> Unit,
) {
    TopAppBar(
        title = {
            Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Text(stringResource(titleForSection(selectedSection)))
                deviceLabel
                    ?.takeIf { it.isNotBlank() }
                    ?.let { label ->
                        Text(
                            text = label,
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                }
            }
        },
        navigationIcon = {
            if (onNavigateBack != null) {
                TextButton(onClick = onNavigateBack) {
                    Text(stringResource(R.string.navigate_back))
                }
            }
        },
        actions = {
            TitleLatencyIndicator(
                status = titleLatencyStatus,
                onClick = onOpenConnectionDiagnostics,
            )
            TextButton(onClick = onExportDiagnosticLog) {
                Text(stringResource(R.string.export_diagnostic_log))
            }
            actions()
        },
    )
}

@Composable
private fun TitleLatencyIndicator(
    status: TitleLatencyProbeStatus,
    onClick: () -> Unit,
) {
    val text = titleLatencyIndicatorText(status) ?: return
    val color = when {
        status.state == "failed" -> MaterialTheme.colorScheme.error
        status.state == "success" && status.connectionType == "direct" -> Color(0xFF16835A)
        status.state == "success" && status.connectionType == "direct_quic" -> Color(0xFF0A6F8F)
        status.state == "success" && status.connectionType == "relay" -> Color(0xFFB85A00)
        else -> MaterialTheme.colorScheme.onSurfaceVariant
    }
    val description = stringResource(
        when (status.connectionType) {
            "direct" -> R.string.title_latency_indicator_direct
            "direct_quic" -> R.string.title_latency_indicator_direct_quic
            "relay" -> R.string.title_latency_indicator_relay
            else -> R.string.title_latency_indicator_unknown
        },
        text,
    )

    TextButton(
        onClick = onClick,
        contentPadding = PaddingValues(horizontal = 4.dp),
        modifier = Modifier.semantics { contentDescription = description },
    ) {
        Text(
            text = text,
            style = MaterialTheme.typography.labelSmall,
            color = color,
        )
    }
}

internal fun titleLatencyIndicatorText(status: TitleLatencyProbeStatus): String? {
    return when (status.state) {
        "disabled" -> null
        "success" -> {
            val connectionPrefix = when (status.connectionType) {
                "direct" -> "D"
                "direct_quic" -> "Q"
                "relay" -> "R"
                else -> "?"
            }
            "$connectionPrefix ${status.latencyMs?.roundToInt() ?: "?"} ms"
        }
        "pending" -> "..."
        else -> "--"
    }
}

private data class ShellItem(
    val section: MainSection,
    val labelRes: Int,
)

private fun shellItems(): List<ShellItem> = listOf(
    ShellItem(MainSection.HOME, R.string.nav_home),
    ShellItem(MainSection.SYNC, R.string.nav_sync),
    ShellItem(MainSection.LIBRARY, R.string.nav_library),
    ShellItem(MainSection.GALLERY_MAP, R.string.nav_gallery_map),
    ShellItem(MainSection.SETTINGS, R.string.nav_settings),
)

private fun titleForSection(section: MainSection): Int {
    return when (section) {
        MainSection.HOME -> R.string.nav_home
        MainSection.CONNECTIVITY -> R.string.nav_connectivity
        MainSection.REQUEST_TIMINGS -> R.string.nav_request_timings
        MainSection.SYNC -> R.string.nav_sync
        MainSection.LIBRARY -> R.string.nav_library
        MainSection.GALLERY_MAP -> R.string.nav_gallery_map
        MainSection.SETTINGS -> R.string.nav_settings
    }
}
