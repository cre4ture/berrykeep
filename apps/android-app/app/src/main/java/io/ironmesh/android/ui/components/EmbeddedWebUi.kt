package io.ironmesh.android.ui.components

import android.annotation.SuppressLint
import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
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
import android.widget.Toast
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.viewinterop.AndroidView
import androidx.webkit.WebMessageCompat
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import io.ironmesh.android.data.EmbeddedWebUiSession
import io.ironmesh.android.data.PrivateWebServiceBrowserSession
import io.ironmesh.android.PrivateWebServiceActivity
import io.ironmesh.android.share.OriginalShareCoordinator
import java.net.URI
import java.util.concurrent.Executors
import org.json.JSONObject

private const val EMBEDDED_WEB_UI_SESSION_HEADER = "X-IronMesh-Web-Ui-Session"
private const val EMBEDDED_WEB_UI_CLIENT_PARAMETER = "embedded_client"
private const val ANDROID_WEB_UI_CLIENT = "android"
private const val ANDROID_SHARE_JAVASCRIPT_OBJECT = "IronmeshAndroidShare"
private const val ANDROID_UI_JAVASCRIPT_OBJECT = "IronmeshAndroidUi"
private const val GALLERY_MAP_FULLSCREEN_MESSAGE_TYPE = "gallery-map-fullscreen"
private const val GALLERY_MAP_FULLSCREEN_EXIT_EVENT = "ironmesh:gallery-map-exit-fullscreen"
private val androidShareExecutor = Executors.newSingleThreadExecutor { runnable ->
    Thread(runnable, "ironmesh-original-share").apply { isDaemon = true }
}

@Composable
fun IronmeshEmbeddedWebUi(
    session: EmbeddedWebUiSession,
    modifier: Modifier = Modifier,
    onCreated: ((WebView) -> Unit)? = null,
    onGalleryMapFullscreenChanged: ((Boolean) -> Unit)? = null,
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
                configureEmbeddedWebUi(
                    session = embeddedSession,
                    onGalleryMapFullscreenChanged = onGalleryMapFullscreenChanged,
                )
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
private fun WebView.configureEmbeddedWebUi(
    session: EmbeddedWebUiSession,
    onGalleryMapFullscreenChanged: ((Boolean) -> Unit)?,
) {
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
    installAndroidShareBridge(session.url)
    installGalleryMapFullscreenBridge(session.url, onGalleryMapFullscreenChanged)
    loadEmbeddedWebUi(session)
}

/** Requests that the direct gallery-map surface leaves its transient fullscreen mode. */
fun WebView.requestGalleryMapFullscreenExit() {
    evaluateJavascript(
        "window.dispatchEvent(new Event('$GALLERY_MAP_FULLSCREEN_EXIT_EVENT'));",
        null,
    )
}

private fun WebView.installAndroidShareBridge(initialUrl: String) {
    if (!WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_LISTENER)) {
        EmbeddedWebUiDiagnostics.report(
            EmbeddedWebUiDiagnostic(
                level = EmbeddedWebUiDiagnosticLevel.WARN,
                event = "android_share_bridge_unavailable",
                pageUrl = initialUrl,
                detail = "The installed Android WebView does not support origin-scoped web messages",
            ),
        )
        return
    }

    val allowedOrigin = embeddedWebUiOrigin(initialUrl)
    val coordinator = OriginalShareCoordinator(context)
    WebViewCompat.addWebMessageListener(
        this,
        ANDROID_SHARE_JAVASCRIPT_OBJECT,
        setOf(allowedOrigin),
    ) { webView, message, sourceOrigin, isMainFrame, replyProxy ->
        val trustedMessage =
            isMainFrame &&
                isSameEmbeddedWebUiOrigin(initialUrl, sourceOrigin.toString()) &&
                message.type == WebMessageCompat.TYPE_STRING
        if (!trustedMessage) {
            replyProxy.postMessage(untrustedShareResponse())
            return@addWebMessageListener
        }

        val rawMessage = message.data.orEmpty()
        androidShareExecutor.execute {
            val preparation = coordinator.prepareWebMessage(rawMessage)
            val posted = webView.post {
                replyProxy.postMessage(coordinator.launchPreparedShare(preparation))
            }
            if (!posted) {
                coordinator.discardPreparedShare(preparation)
            }
        }
    }
}

private fun WebView.installGalleryMapFullscreenBridge(
    initialUrl: String,
    onGalleryMapFullscreenChanged: ((Boolean) -> Unit)?,
) {
    if (onGalleryMapFullscreenChanged == null) {
        return
    }
    if (!WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_LISTENER)) {
        EmbeddedWebUiDiagnostics.report(
            EmbeddedWebUiDiagnostic(
                level = EmbeddedWebUiDiagnosticLevel.WARN,
                event = "android_gallery_map_fullscreen_bridge_unavailable",
                pageUrl = initialUrl,
                detail = "The installed Android WebView does not support origin-scoped web messages",
            ),
        )
        return
    }

    val allowedOrigin = embeddedWebUiOrigin(initialUrl)
    WebViewCompat.addWebMessageListener(
        this,
        ANDROID_UI_JAVASCRIPT_OBJECT,
        setOf(allowedOrigin),
    ) { webView, message, sourceOrigin, isMainFrame, _ ->
        val trustedMessage =
            isMainFrame &&
                isSameEmbeddedWebUiOrigin(initialUrl, sourceOrigin.toString()) &&
                message.type == WebMessageCompat.TYPE_STRING
        if (!trustedMessage) {
            return@addWebMessageListener
        }

        val isFullscreen = parseGalleryMapFullscreenMessage(message.data) ?: return@addWebMessageListener
        webView.post {
            onGalleryMapFullscreenChanged(isFullscreen)
        }
    }
}

private fun parseGalleryMapFullscreenMessage(rawMessage: String?): Boolean? {
    val payload = runCatching { JSONObject(rawMessage.orEmpty()) }.getOrNull() ?: return null
    if (payload.optString("type") != GALLERY_MAP_FULLSCREEN_MESSAGE_TYPE) {
        return null
    }
    return payload.opt("fullscreen") as? Boolean
}

private fun untrustedShareResponse(): String =
    """{"requestId":"","status":"error","message":"Rejected untrusted share request"}"""

private fun embeddedWebUiOrigin(url: String): String {
    val parsed = URI(url)
    require(
        parsed.scheme.equals("http", ignoreCase = true) ||
            parsed.scheme.equals("https", ignoreCase = true),
    ) { "Embedded Web UI origin must use HTTP or HTTPS" }
    require(!parsed.host.isNullOrBlank()) { "Embedded Web UI origin has no host" }
    return URI(parsed.scheme.lowercase(), null, parsed.host, parsed.port, null, null, null).toString()
}

private fun WebView.loadEmbeddedWebUi(session: EmbeddedWebUiSession) {
    loadUrl(session.url, mapOf(EMBEDDED_WEB_UI_SESSION_HEADER to session.authorization))
}

private class EmbeddedWebUiClient(
    private val initialUrl: String,
) : WebViewClient() {
    override fun shouldOverrideUrlLoading(view: WebView, url: String): Boolean =
        shouldOverrideNavigation(view, url)

    override fun shouldOverrideUrlLoading(
        view: WebView,
        request: WebResourceRequest,
    ): Boolean = shouldOverrideNavigation(view, request.url.toString())

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

    private fun shouldOverrideNavigation(view: WebView, candidateUrl: String): Boolean {
        val privateServiceLaunch = parsePrivateWebServiceLaunch(initialUrl, candidateUrl)
        if (privateServiceLaunch != null) {
            openPrivateService(view, privateServiceLaunch)
            return true
        }
        if (isPotentialPrivateWebServiceLaunchUrl(candidateUrl)) {
            reportPrivateServiceLaunchFailure(
                view,
                candidateUrl,
                "Private-service launch URL did not match this embedded listener",
            )
        }
        return !isSameEmbeddedWebUiOrigin(initialUrl, candidateUrl)
    }

    private fun openPrivateService(view: WebView, launch: PrivateWebServiceLaunch) {
        when (launch.target) {
            PrivateWebServiceOpenTarget.IN_APP -> {
                view.context.startActivity(PrivateWebServiceActivity.intent(view.context, launch.url))
            }

            PrivateWebServiceOpenTarget.BROWSER -> {
                if (!PrivateWebServiceBrowserSession.acquire(view.context)) {
                    reportPrivateServiceLaunchFailure(view, launch.url, "Could not keep the local proxy active")
                    return
                }
                try {
                    view.context.startActivity(
                        Intent(Intent.ACTION_VIEW, Uri.parse(launch.url))
                            .addCategory(Intent.CATEGORY_BROWSABLE)
                            .apply {
                                if (view.context !is Activity) {
                                    addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                                }
                            },
                    )
                } catch (error: ActivityNotFoundException) {
                    PrivateWebServiceBrowserSession.release(view.context)
                    reportPrivateServiceLaunchFailure(
                        view,
                        launch.url,
                        error.message ?: "No browser is available for the private service",
                    )
                }
            }
        }
    }

    private fun reportPrivateServiceLaunchFailure(view: WebView, requestUrl: String, detail: String) {
        EmbeddedWebUiDiagnostics.report(
            EmbeddedWebUiDiagnostic(
                level = EmbeddedWebUiDiagnosticLevel.ERROR,
                event = "private_service_launch_failed",
                pageUrl = view.url,
                requestUrl = requestUrl,
                detail = detail,
            ),
        )
        Toast.makeText(view.context, "Could not open private service", Toast.LENGTH_LONG).show()
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
