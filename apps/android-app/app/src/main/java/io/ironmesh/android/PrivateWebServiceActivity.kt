package io.ironmesh.android

import android.content.Context
import android.content.Intent
import android.webkit.WebChromeClient
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import io.ironmesh.android.ui.components.EmbeddedWebUiDiagnostics
import io.ironmesh.android.ui.components.EmbeddedWebUiDiagnostic
import io.ironmesh.android.ui.components.EmbeddedWebUiDiagnosticLevel
import io.ironmesh.android.ui.components.EmbeddedWebUiDownloadListener
import io.ironmesh.android.ui.components.isPrivateWebServiceLaunchUrl
import io.ironmesh.android.ui.components.isSameEmbeddedWebUiOrigin

/** Displays one issued private-service origin in an isolated BerryKeep WebView. */
class PrivateWebServiceActivity : ComponentActivity() {
    private var hostedWebView: WebView? = null
    private var launchUrl: String? = null

    override fun onCreate(savedInstanceState: android.os.Bundle?) {
        super.onCreate(savedInstanceState)
        val launchUrl = intent.getStringExtra(EXTRA_LAUNCH_URL).orEmpty()
        if (!isPrivateWebServiceLaunchUrl(launchUrl)) {
            finish()
            return
        }
        this.launchUrl = launchUrl
        hostedWebView = WebView(this).apply {
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            settings.allowFileAccess = false
            settings.allowContentAccess = false
            settings.mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
            settings.javaScriptCanOpenWindowsAutomatically = false
            settings.setSupportMultipleWindows(false)
            webViewClient = PrivateWebServiceClient(launchUrl)
            webChromeClient = WebChromeClient()
            setDownloadListener(EmbeddedWebUiDownloadListener(context, launchUrl))
            loadUrl(launchUrl)
        }
        setContentView(requireNotNull(hostedWebView))
        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                val webView = hostedWebView
                if (webView != null && webView.canGoBack()) {
                    webView.goBack()
                } else {
                    finish()
                }
            }
        })
    }

    companion object {
        private const val EXTRA_LAUNCH_URL = "io.ironmesh.android.extra.PRIVATE_WEB_SERVICE_LAUNCH_URL"

        fun intent(context: Context, launchUrl: String): Intent =
            Intent(context, PrivateWebServiceActivity::class.java)
                .putExtra(EXTRA_LAUNCH_URL, launchUrl)
    }
}

private class PrivateWebServiceClient(
    private val launchUrl: String,
) : WebViewClient() {
    override fun shouldOverrideUrlLoading(view: WebView, url: String): Boolean =
        rejectCrossOriginNavigation(view, url)

    override fun shouldOverrideUrlLoading(
        view: WebView,
        request: WebResourceRequest,
    ): Boolean = rejectCrossOriginNavigation(view, request.url.toString())

    override fun onReceivedError(
        view: WebView,
        request: WebResourceRequest,
        error: WebResourceError,
    ) {
        super.onReceivedError(view, request, error)
        if (request.isForMainFrame) {
            EmbeddedWebUiDiagnostics.report(
                EmbeddedWebUiDiagnostic(
                    level = EmbeddedWebUiDiagnosticLevel.ERROR,
                    event = "private_service_resource_error",
                    pageUrl = view.url,
                    requestUrl = request.url.toString(),
                    method = request.method,
                    errorCode = error.errorCode,
                    detail = error.description?.toString(),
                ),
            )
        }
    }

    private fun rejectCrossOriginNavigation(view: WebView, candidateUrl: String): Boolean {
        if (isSameEmbeddedWebUiOrigin(launchUrl, candidateUrl)) {
            return false
        }
        EmbeddedWebUiDiagnostics.report(
            EmbeddedWebUiDiagnostic(
                level = EmbeddedWebUiDiagnosticLevel.WARN,
                event = "private_service_navigation_blocked",
                pageUrl = view.url,
                requestUrl = candidateUrl,
                detail = "Private-service WebView rejected a cross-origin navigation",
            ),
        )
        return true
    }
}
