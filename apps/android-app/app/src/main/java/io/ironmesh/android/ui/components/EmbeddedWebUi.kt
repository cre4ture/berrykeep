package io.ironmesh.android.ui.components

import android.annotation.SuppressLint
import android.net.Uri
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.viewinterop.AndroidView
import io.ironmesh.android.data.EmbeddedWebUiSession

private const val EMBEDDED_WEB_UI_SESSION_HEADER = "X-IronMesh-Web-Ui-Session"
private const val EMBEDDED_WEB_UI_CLIENT_PARAMETER = "embedded_client"
private const val ANDROID_WEB_UI_CLIENT = "android"

@Composable
fun IronmeshEmbeddedWebUi(
    session: EmbeddedWebUiSession,
    modifier: Modifier = Modifier,
    onCreated: ((WebView) -> Unit)? = null,
) {
    val embeddedSession = session.withUrl(
        Uri.parse(session.url)
            .buildUpon()
            .appendQueryParameter(EMBEDDED_WEB_UI_CLIENT_PARAMETER, ANDROID_WEB_UI_CLIENT)
            .build()
            .toString(),
    )
    AndroidView(
        modifier = modifier,
        factory = { context ->
            WebView(context).apply {
                configureEmbeddedWebUi(embeddedSession)
                onCreated?.invoke(this)
            }
        },
        update = { webView ->
            if (webView.url != embeddedSession.url) {
                webView.loadEmbeddedWebUi(embeddedSession)
            }
        },
    )
}

@SuppressLint("SetJavaScriptEnabled")
private fun WebView.configureEmbeddedWebUi(session: EmbeddedWebUiSession) {
    settings.javaScriptEnabled = true
    settings.domStorageEnabled = true
    settings.allowFileAccess = false
    settings.allowContentAccess = false
    settings.mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
    settings.javaScriptCanOpenWindowsAutomatically = false
    settings.setSupportMultipleWindows(false)
    webViewClient = EmbeddedWebUiClient(session.url)
    loadEmbeddedWebUi(session)
}

private fun WebView.loadEmbeddedWebUi(session: EmbeddedWebUiSession) {
    loadUrl(session.url, mapOf(EMBEDDED_WEB_UI_SESSION_HEADER to session.authorization))
}

private class EmbeddedWebUiClient(
    private val initialUrl: String,
) : WebViewClient() {
    override fun shouldOverrideUrlLoading(view: WebView, url: String): Boolean = !isSameOrigin(url)

    override fun shouldOverrideUrlLoading(
        view: WebView,
        request: android.webkit.WebResourceRequest,
    ): Boolean = !isSameOrigin(request.url.toString())

    private fun isSameOrigin(url: String): Boolean {
        val origin = Uri.parse(initialUrl)
        val candidate = Uri.parse(url)
        return candidate.scheme == origin.scheme &&
            candidate.host == origin.host &&
            candidate.port == origin.port
    }
}
