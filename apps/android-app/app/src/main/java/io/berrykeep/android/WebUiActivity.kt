package io.berrykeep.android

import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.webkit.WebView
import androidx.activity.ComponentActivity
import androidx.activity.addCallback
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.ui.Modifier
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import io.berrykeep.android.data.EmbeddedWebUiSession
import io.berrykeep.android.data.EmbeddedWebUiSessionRegistry
import io.berrykeep.android.data.BerryKeepPreferences
import io.berrykeep.android.ui.components.BerryKeepEmbeddedWebUi

class WebUiActivity : ComponentActivity() {
    private var hostedWebView: WebView? = null
    private var session: EmbeddedWebUiSession? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        hideStatusBar()

        onBackPressedDispatcher.addCallback(this) {
            val webView = hostedWebView
            if (webView != null && webView.canGoBack()) {
                webView.goBack()
            } else {
                finish()
            }
        }

        val url = intent.getStringExtra(EXTRA_WEB_UI_URL).orEmpty()
        val authorization = intent.getStringExtra(EXTRA_WEB_UI_AUTHORIZATION).orEmpty()
        if (url.isBlank() || authorization.isBlank()) {
            finish()
            return
        }
        val session = EmbeddedWebUiSession(url, authorization)
        this.session = session

        setContent {
            MaterialTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    BerryKeepEmbeddedWebUi(
                        modifier = Modifier.fillMaxSize(),
                        session = session,
                        accentColorHex = BerryKeepPreferences.getThemeAccentColor(this),
                        onCreated = { hostedWebView = it },
                    )
                }
            }
        }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) {
            hideStatusBar()
        }
    }

    override fun onResume() {
        super.onResume()
        val currentSession = session
        if (currentSession != null && !EmbeddedWebUiSessionRegistry.isActive(currentSession)) {
            finish()
        }
    }

    private fun hideStatusBar() {
        WindowInsetsControllerCompat(window, window.decorView).apply {
            systemBarsBehavior =
                WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            hide(WindowInsetsCompat.Type.statusBars())
        }
    }

    companion object {
        private const val EXTRA_WEB_UI_URL = "io.berrykeep.android.extra.WEB_UI_URL"
        private const val EXTRA_WEB_UI_AUTHORIZATION = "io.berrykeep.android.extra.WEB_UI_AUTHORIZATION"

        fun intent(context: Context, session: EmbeddedWebUiSession): Intent =
            Intent(context, WebUiActivity::class.java)
                .putExtra(EXTRA_WEB_UI_URL, session.url)
                .putExtra(EXTRA_WEB_UI_AUTHORIZATION, session.authorization)
    }
}
