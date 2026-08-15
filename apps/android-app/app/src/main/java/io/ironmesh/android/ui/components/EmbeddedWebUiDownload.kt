package io.ironmesh.android.ui.components

import android.app.DownloadManager
import android.content.Context
import android.net.Uri
import android.webkit.CookieManager
import android.webkit.DownloadListener
import android.widget.Toast
import java.net.URI
import java.net.URLDecoder
import java.nio.charset.StandardCharsets

internal class EmbeddedWebUiDownloadListener(
    private val context: Context,
    private val initialUrl: String,
) : DownloadListener {
    override fun onDownloadStart(
        url: String,
        userAgent: String,
        contentDisposition: String,
        mimetype: String,
        contentLength: Long,
    ) {
        if (!isSameEmbeddedWebUiOrigin(initialUrl, url)) {
            reportDownloadFailure("download_rejected", url, "Download URL is outside the embedded Web UI origin")
            Toast.makeText(context, "Download blocked", Toast.LENGTH_SHORT).show()
            return
        }

        val fileName = embeddedWebUiDownloadFileName(url, contentDisposition)
        runCatching {
            val request = DownloadManager.Request(Uri.parse(url))
                .setTitle(fileName)
                .setDescription("Downloading original media")
                .setNotificationVisibility(DownloadManager.Request.VISIBILITY_VISIBLE_NOTIFY_COMPLETED)

            if (mimetype.isNotBlank()) {
                request.setMimeType(mimetype)
            }
            if (userAgent.isNotBlank()) {
                request.addRequestHeader("User-Agent", userAgent)
            }
            CookieManager.getInstance().getCookie(url)?.takeIf(String::isNotBlank)?.let { cookie ->
                request.addRequestHeader("Cookie", cookie)
            }

            val downloadManager = requireNotNull(context.getSystemService(DownloadManager::class.java)) {
                "Android download manager is unavailable"
            }
            downloadManager.enqueue(request)
        }.onSuccess {
            Toast.makeText(context, "Downloading $fileName", Toast.LENGTH_SHORT).show()
        }.onFailure { error ->
            reportDownloadFailure("download_enqueue_failed", url, error.message ?: error.javaClass.simpleName)
            Toast.makeText(context, "Download failed to start", Toast.LENGTH_SHORT).show()
        }
    }
}

internal fun isSameEmbeddedWebUiOrigin(initialUrl: String, candidateUrl: String): Boolean =
    runCatching {
        val origin = URI(initialUrl)
        val candidate = URI(candidateUrl)
        origin.scheme.equals(candidate.scheme, ignoreCase = true) &&
            origin.host.equals(candidate.host, ignoreCase = true) &&
            effectivePort(origin) == effectivePort(candidate)
    }.getOrDefault(false)

internal fun embeddedWebUiDownloadFileName(url: String, contentDisposition: String): String {
    val dispositionName = Regex(
        pattern = """(?:^|;)\s*filename\s*=\s*(?:\"([^\"]*)\"|([^;]*))""",
        option = RegexOption.IGNORE_CASE,
    ).find(contentDisposition)?.let { match ->
        match.groupValues[1].ifBlank { match.groupValues[2] }
    }
    val queryName = runCatching {
        URI(url).rawQuery
            ?.split('&')
            ?.firstNotNullOfOrNull { parameter ->
                val (name, value) = parameter.split('=', limit = 2).let { parts ->
                    parts.first() to parts.getOrElse(1) { "" }
                }
                if (name == "key") {
                    URLDecoder.decode(value, StandardCharsets.UTF_8.name())
                } else {
                    null
                }
            }
    }.getOrNull()
    val pathName = runCatching { URI(url).path.substringAfterLast('/') }.getOrNull()

    return sanitizeEmbeddedWebUiDownloadFileName(
        dispositionName?.ifBlank { null } ?: queryName ?: pathName.orEmpty(),
    )
}

private fun effectivePort(uri: URI): Int =
    when {
        uri.port >= 0 -> uri.port
        uri.scheme.equals("http", ignoreCase = true) -> 80
        uri.scheme.equals("https", ignoreCase = true) -> 443
        else -> -1
    }

private fun sanitizeEmbeddedWebUiDownloadFileName(candidate: String): String {
    val leafName = candidate.substringAfterLast('/').substringAfterLast('\\').trim()
    val sanitized = buildString(leafName.length) {
        leafName.forEach { character ->
            append(
                if (character.code < 32 || character in "<>:\"/\\|?*") {
                    '_'
                } else {
                    character
                },
            )
        }
    }.trim().trim('.')
    return sanitized.takeIf { it.isNotBlank() } ?: "download.bin"
}

private fun reportDownloadFailure(event: String, requestUrl: String, detail: String) {
    EmbeddedWebUiDiagnostics.report(
        EmbeddedWebUiDiagnostic(
            level = EmbeddedWebUiDiagnosticLevel.WARN,
            event = event,
            requestUrl = requestUrl,
            detail = detail,
        ),
    )
}
