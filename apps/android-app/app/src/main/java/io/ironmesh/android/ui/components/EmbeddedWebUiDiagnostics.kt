package io.ironmesh.android.ui.components

import io.ironmesh.android.data.AndroidDiagnosticLog as Log
import io.ironmesh.android.data.RustClientBridge
import java.net.URI

private const val WEB_UI_DIAGNOSTIC_TAG = "EmbeddedWebUi"
private const val MAX_DIAGNOSTIC_DETAIL_LENGTH = 384
private const val MAX_DEDUPLICATED_EVENTS = 128
private const val DUPLICATE_EVENT_WINDOW_MS = 10_000L

internal enum class EmbeddedWebUiDiagnosticLevel {
    WARN,
    ERROR,
}

/**
 * A compact, non-secret diagnostic emitted by the Android WebView wrapper.
 *
 * URLs are deliberately reduced to paths, and arbitrary details are redacted and bounded before
 * they reach either retained diagnostic buffer.
 */
internal data class EmbeddedWebUiDiagnostic(
    val level: EmbeddedWebUiDiagnosticLevel,
    val event: String,
    val pageUrl: String? = null,
    val requestUrl: String? = null,
    val method: String? = null,
    val statusCode: Int? = null,
    val errorCode: Int? = null,
    val scriptUrl: String? = null,
    val lineNumber: Int? = null,
    val detail: String? = null,
) {
    fun render(): String = buildString {
        append("event=").append(event)
        diagnosticPath(pageUrl)?.let { append(" page_path=").append(it) }
        diagnosticPath(requestUrl)?.let { append(" request_path=").append(it) }
        method?.trim()?.uppercase()?.takeIf { it.isNotEmpty() }?.let {
            append(" method=").append(it)
        }
        statusCode?.takeIf { it in 100..599 }?.let { append(" status=").append(it) }
        errorCode?.let { append(" error_code=").append(it) }
        diagnosticPath(scriptUrl)?.let { append(" script_path=").append(it) }
        lineNumber?.takeIf { it >= 0 }?.let { append(" line=").append(it) }
        sanitizeDiagnosticDetail(detail)?.let { append(" detail=").append(it) }
    }
}

/**
 * Keeps the Android and native diagnostic exports aligned without allowing a noisy WebView to
 * exhaust their bounded buffers with the same resource failure.
 */
internal class EmbeddedWebUiDiagnosticReporter(
    private val nowUnixMs: () -> Long = System::currentTimeMillis,
    private val emit: (EmbeddedWebUiDiagnosticLevel, String) -> Unit,
) {
    private val recentlyReported = LinkedHashMap<String, Long>()

    @Synchronized
    fun report(diagnostic: EmbeddedWebUiDiagnostic) {
        val message = diagnostic.render()
        if (!shouldEmit(message)) {
            return
        }
        emit(diagnostic.level, message)
    }

    private fun shouldEmit(message: String): Boolean {
        val now = nowUnixMs()
        val lastReportedAt = recentlyReported[message]
        if (lastReportedAt != null && now - lastReportedAt in 0 until DUPLICATE_EVENT_WINDOW_MS) {
            return false
        }

        recentlyReported[message] = now
        while (recentlyReported.size > MAX_DEDUPLICATED_EVENTS) {
            recentlyReported.entries.iterator().apply {
                if (hasNext()) {
                    next()
                    remove()
                }
            }
        }
        return true
    }
}

internal object EmbeddedWebUiDiagnostics {
    private val reporter = EmbeddedWebUiDiagnosticReporter { level, message ->
        when (level) {
            EmbeddedWebUiDiagnosticLevel.WARN -> Log.w(WEB_UI_DIAGNOSTIC_TAG, message)
            EmbeddedWebUiDiagnosticLevel.ERROR -> Log.e(WEB_UI_DIAGNOSTIC_TAG, message)
        }
        runCatching { RustClientBridge.recordEmbeddedWebUiDiagnostic(message) }
            .onFailure { error ->
                Log.w(
                    WEB_UI_DIAGNOSTIC_TAG,
                    "Failed to mirror embedded Web UI diagnostic into native log",
                    error,
                )
            }
    }

    fun report(diagnostic: EmbeddedWebUiDiagnostic) = reporter.report(diagnostic)
}

internal fun isEmbeddedWebUiApiRequest(url: String?): Boolean =
    diagnosticPath(url)?.startsWith("/api/") == true

private fun diagnosticPath(url: String?): String? {
    val rawUrl = url?.trim()?.takeIf { it.isNotEmpty() } ?: return null
    val path = runCatching { URI(rawUrl).rawPath }.getOrNull()
        ?: rawUrl.substringBefore('?').substringBefore('#')
    return path
        .trim()
        .takeIf { it.isNotEmpty() }
        ?.take(256)
}

private fun sanitizeDiagnosticDetail(detail: String?): String? {
    val normalized = detail?.trim()?.takeIf { it.isNotEmpty() } ?: return null
    val withoutUrls = URL_PATTERN.replace(normalized) { match ->
        diagnosticPath(match.value) ?: "<redacted-url>"
    }
    val withoutSecrets = SECRET_VALUE_PATTERN.replace(withoutUrls) { match ->
        "${match.groupValues[1]}${match.groupValues[2]}<redacted>"
    }
    val withoutBearerTokens = BEARER_TOKEN_PATTERN.replace(withoutSecrets, "Bearer <redacted>")
    return WHITESPACE_PATTERN.replace(withoutBearerTokens, " ")
        .replace('"', '\'')
        .trim()
        .take(MAX_DIAGNOSTIC_DETAIL_LENGTH)
        .takeIf { it.isNotEmpty() }
}

private val URL_PATTERN = Regex("https?://[^\\s\\\"'<>]+", RegexOption.IGNORE_CASE)
private val SECRET_VALUE_PATTERN = Regex(
    "\\b(authorization|token|password|secret|cookie|session)(\\s*[:=]\\s*)[^\\s,;]+",
    RegexOption.IGNORE_CASE,
)
private val BEARER_TOKEN_PATTERN = Regex("\\bBearer\\s+[A-Za-z0-9._~+/-]+=*", RegexOption.IGNORE_CASE)
private val WHITESPACE_PATTERN = Regex("\\s+")
