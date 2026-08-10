package io.ironmesh.android

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.Surface
import androidx.compose.ui.Modifier
import io.ironmesh.android.data.EmbeddedWebUiSession
import io.ironmesh.android.ui.GalleryMapUiState
import io.ironmesh.android.ui.screens.GalleryMapScreen
import io.ironmesh.android.ui.theme.IronmeshTheme

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
            IronmeshTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    GalleryMapScreen(
                        state = GalleryMapUiState(
                            webUiSession = EmbeddedWebUiSession(url, authorization),
                            loading = false,
                            status = "Ready",
                        ),
                        onStartGalleryMap = {},
                    )
                }
            }
        }
    }

    companion object {
        const val EXTRA_WEB_UI_URL = "io.ironmesh.android.extra.GALLERY_MAP_TEST_WEB_UI_URL"
        const val EXTRA_WEB_UI_AUTHORIZATION =
            "io.ironmesh.android.extra.GALLERY_MAP_TEST_WEB_UI_AUTHORIZATION"
    }
}
