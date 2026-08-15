package io.ironmesh.android.ui.components

import android.annotation.SuppressLint
import android.net.Uri
import android.net.http.SslError
import android.webkit.ConsoleMessage
import android.webkit.SslErrorHandler
import android.webkit.WebChromeClient
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
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
    webChromeClient = EmbeddedWebUiChromeClient()
    setDownloadListener(EmbeddedWebUiDownloadListener(context, session.url))
    loadEmbeddedWebUi(session)
}

private fun WebView.loadEmbeddedWebUi(session: EmbeddedWebUiSession) {
    loadUrl(session.url, mapOf(EMBEDDED_WEB_UI_SESSION_HEADER to session.authorization))
}

private class EmbeddedWebUiClient(
    private val initialUrl: String,
) : WebViewClient() {
    override fun shouldOverrideUrlLoading(view: WebView, url: String): Boolean =
        !isSameEmbeddedWebUiOrigin(initialUrl, url)

    override fun shouldOverrideUrlLoading(
        view: WebView,
        request: WebResourceRequest,
    ): Boolean = !isSameEmbeddedWebUiOrigin(initialUrl, request.url.toString())

    override fun onReceivedHttpError(
        view: WebView,
        request: WebResourceRequest,
        errorResponse: WebResourceResponse,
    ) {
        super.onReceivedHttpError(view, request, errorResponse)
        if (request.isForMainFrame || isEmbeddedWebUiApiRequest(request.url.toString())) {
            EmbeddedWebUiDiagnostics.report(
                EmbeddedWebUiDiagnostic(
                    level = EmbeddedWebUiDiagnosticLevel.WARN,
                    event = "http_error",
                    pageUrl = view.url,
                    requestUrl = request.url.toString(),
                    method = request.method,
                    statusCode = errorResponse.statusCode,
                    detail = errorResponse.reasonPhrase,
                ),
            )
        }
    }

    override fun onReceivedError(
        view: WebView,
        request: WebResourceRequest,
        error: WebResourceError,
    ) {
        super.onReceivedError(view, request, error)
        if (request.isForMainFrame || isEmbeddedWebUiApiRequest(request.url.toString())) {
            EmbeddedWebUiDiagnostics.report(
                EmbeddedWebUiDiagnostic(
                    level = EmbeddedWebUiDiagnosticLevel.ERROR,
                    event = "resource_error",
                    pageUrl = view.url,
                    requestUrl = request.url.toString(),
                    method = request.method,
                    errorCode = error.errorCode,
                    detail = error.description?.toString(),
                ),
            )
        }
    }

    override fun onReceivedSslError(
        view: WebView,
        handler: SslErrorHandler,
        error: SslError,
    ) {
        super.onReceivedSslError(view, handler, error)
        EmbeddedWebUiDiagnostics.report(
            EmbeddedWebUiDiagnostic(
                level = EmbeddedWebUiDiagnosticLevel.ERROR,
                event = "ssl_error",
                pageUrl = view.url,
                requestUrl = error.url,
                errorCode = error.primaryError,
            ),
        )
    }
}

private class EmbeddedWebUiChromeClient : WebChromeClient() {
    override fun onConsoleMessage(consoleMessage: ConsoleMessage): Boolean {
        val level = when (consoleMessage.messageLevel()) {
            ConsoleMessage.MessageLevel.ERROR -> EmbeddedWebUiDiagnosticLevel.ERROR
            ConsoleMessage.MessageLevel.WARNING -> EmbeddedWebUiDiagnosticLevel.WARN
            else -> return super.onConsoleMessage(consoleMessage)
        }
        EmbeddedWebUiDiagnostics.report(
            EmbeddedWebUiDiagnostic(
                level = level,
                event = "javascript_console",
                scriptUrl = consoleMessage.sourceId(),
                lineNumber = consoleMessage.lineNumber(),
                detail = consoleMessage.message(),
            ),
        )
        return super.onConsoleMessage(consoleMessage)
    }
}
