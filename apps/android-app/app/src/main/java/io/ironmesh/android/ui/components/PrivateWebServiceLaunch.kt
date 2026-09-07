package io.ironmesh.android.ui.components

import java.net.URI
import java.net.URLDecoder
import java.nio.charset.StandardCharsets
import java.util.Locale

internal enum class PrivateWebServiceOpenTarget {
    IN_APP,
    BROWSER,
}

internal data class PrivateWebServiceLaunch(
    val url: String,
    val target: PrivateWebServiceOpenTarget,
)

private const val SERVICE_HOST_SUFFIX = ".localhost"
private const val SERVICE_OPEN_PATH = "/_ironmesh/open"
private const val SERVICE_TOKEN_PARAMETER = "token"
private const val SERVICE_OPEN_TARGET_PARAMETER = "ironmesh_open"
private const val SERVICE_BROWSER_OPEN_TARGET = "browser"

/**
 * Accepts only an issued local private-service launch URL. The client WebView
 * remains confined to its own loopback origin; this is the sole cross-origin
 * handoff it may ask Android to perform.
 */
internal fun parsePrivateWebServiceLaunch(
    initialWebUiUrl: String,
    candidateUrl: String,
): PrivateWebServiceLaunch? {
    val initial = parseUri(initialWebUiUrl) ?: return null
    val candidate = parseUri(candidateUrl) ?: return null
    if (!isLocalWebUiOrigin(initial) || !isPrivateWebServiceLaunchUri(candidate)) {
        return null
    }
    if (effectivePort(initial) != effectivePort(candidate)) {
        return null
    }
    return PrivateWebServiceLaunch(
        url = candidate.toString(),
        target = if (
            queryParameterValues(candidate, SERVICE_OPEN_TARGET_PARAMETER)
                ?.firstOrNull() == SERVICE_BROWSER_OPEN_TARGET
        ) {
            PrivateWebServiceOpenTarget.BROWSER
        } else {
            PrivateWebServiceOpenTarget.IN_APP
        },
    )
}

internal fun isPrivateWebServiceLaunchUrl(url: String): Boolean =
    parseUri(url)?.let(::isPrivateWebServiceLaunchUri) == true

/** Identifies a rejected local launch candidate so the embedded client can explain the refusal. */
internal fun isPotentialPrivateWebServiceLaunchUrl(url: String): Boolean =
    parseUri(url)?.let { uri ->
        uri.scheme.equals("http", ignoreCase = true) &&
            uri.host?.lowercase(Locale.ROOT)?.endsWith(SERVICE_HOST_SUFFIX) == true &&
            (uri.path == SERVICE_OPEN_PATH || uri.path.startsWith("$SERVICE_OPEN_PATH/"))
    } == true

private fun parseUri(value: String): URI? =
    runCatching { URI(value) }
        .getOrNull()
        ?.takeIf { it.scheme != null && it.host != null }

private fun isLocalWebUiOrigin(uri: URI): Boolean {
    if (!uri.scheme.equals("http", ignoreCase = true)) {
        return false
    }
    return uri.host?.lowercase(Locale.ROOT) in setOf("127.0.0.1", "::1", "localhost")
}

private fun isPrivateWebServiceLaunchUri(uri: URI): Boolean {
    if (!uri.scheme.equals("http", ignoreCase = true) || uri.path != SERVICE_OPEN_PATH) {
        return false
    }
    val host = uri.host?.lowercase(Locale.ROOT) ?: return false
    if (!host.endsWith(SERVICE_HOST_SUFFIX)) {
        return false
    }
    val alias = host.removeSuffix(SERVICE_HOST_SUFFIX)
    if (alias.isBlank() || alias.contains('.') || !alias.all(::isServiceAliasCharacter)) {
        return false
    }
    val tokens = queryParameterValues(uri, SERVICE_TOKEN_PARAMETER) ?: return false
    return tokens.size == 1 && tokens.single().isNotBlank()
}

private fun isServiceAliasCharacter(character: Char): Boolean =
    character.code <= 0x7f && (character.isLetterOrDigit() || character == '-')

private fun effectivePort(uri: URI): Int =
    uri.port.takeIf { it >= 0 } ?: 80

private fun queryParameterValues(uri: URI, name: String): List<String>? {
    val rawQuery = uri.rawQuery ?: return emptyList()
    val values = mutableListOf<String>()
    for (rawParameter in rawQuery.split('&')) {
        val parts = rawParameter.split('=', limit = 2)
        val parameterName = decodeQueryComponent(parts.first()) ?: return null
        if (parameterName == name) {
            values += decodeQueryComponent(parts.getOrElse(1) { "" }) ?: return null
        }
    }
    return values
}

private fun decodeQueryComponent(value: String): String? =
    runCatching { URLDecoder.decode(value, StandardCharsets.UTF_8.name()) }.getOrNull()
