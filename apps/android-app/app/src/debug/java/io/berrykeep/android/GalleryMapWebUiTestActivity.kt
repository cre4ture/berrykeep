package io.berrykeep.android

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Box
import androidx.compose.material3.SnackbarHostState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import io.berrykeep.android.data.EmbeddedWebUiSession
import io.berrykeep.android.data.TitleLatencyProbeStatus
import io.berrykeep.android.ui.GalleryMapUiState
import io.berrykeep.android.ui.MainSection
import io.berrykeep.android.ui.components.BerryKeepAppShell
import io.berrykeep.android.ui.screens.GalleryMapScreen
import io.berrykeep.android.ui.theme.BerryKeepTheme

/**
 * Debug-only host for exercising the native gallery-map embedding against a
 * real Client UI runtime from instrumentation tests.
 */
class GalleryMapWebUiTestActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val url = intent.getStringExtra(EXTRA_WEB_UI_URL).orEmpty()
        val authorization = intent.getStringExtra(EXTRA_WEB_UI_AUTHORIZATION).orEmpty()
        check(url.isNotBlank()) { "A Client UI URL is required for the gallery-map test host." }
        check(authorization.isNotBlank()) { "A Client UI authorization is required for the gallery-map test host." }

        setContent {
            BerryKeepTheme {
                var fullscreenContent by remember { mutableStateOf(false) }
                BerryKeepAppShell(
                    selectedSection = MainSection.GALLERY_MAP,
                    onSelectSection = {},
                    snackbarHostState = remember { SnackbarHostState() },
                    deviceLabel = null,
                    titleLatencyStatus = TitleLatencyProbeStatus(),
                    onOpenConnectionDiagnostics = {},
                    onExportDiagnosticLog = {},
                    fullscreenContent = fullscreenContent,
                ) { contentModifier ->
                    Box(modifier = contentModifier) {
                        GalleryMapScreen(
                            state = GalleryMapUiState(
                                webUiSession = EmbeddedWebUiSession(url, authorization),
                                loading = false,
                                status = "Ready",
                            ),
                            onStartGalleryMap = {},
                            onFullscreenChanged = { fullscreenContent = it },
                        )
                    }
                }
            }
        }
    }

    companion object {
        const val EXTRA_WEB_UI_URL = "io.berrykeep.android.extra.GALLERY_MAP_TEST_WEB_UI_URL"
        const val EXTRA_WEB_UI_AUTHORIZATION =
            "io.berrykeep.android.extra.GALLERY_MAP_TEST_WEB_UI_AUTHORIZATION"
    }
}
