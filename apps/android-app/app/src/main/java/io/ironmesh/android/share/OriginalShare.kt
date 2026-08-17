package io.ironmesh.android.share

import android.app.Activity
import android.content.ClipData
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import org.json.JSONObject
import java.io.FileNotFoundException
import java.util.UUID

internal data class OriginalShareRequest(
    val requestId: String,
    val remotePath: String,
    val snapshotId: String?,
    val versionId: String?,
    val displayName: String,
    val mimeType: String,
    val sizeBytes: Long?,
) {
    companion object {
        fun fromWebMessage(rawMessage: String): OriginalShareRequest {
            require(rawMessage.length <= MAX_WEB_MESSAGE_LENGTH) {
                "Android share request is too large"
            }
            val payload = JSONObject(rawMessage)
            require(payload.optString("action") == "share-original") {
                "Unsupported Android share action"
            }
            val requestId = payload.requiredString("requestId", MAX_REQUEST_ID_LENGTH)
            val remotePath = payload
                .requiredString("key", MAX_REMOTE_PATH_LENGTH)
                .trim('/')
                .also { require(it.isNotBlank()) { "The remote path is empty" } }
            val snapshotId = payload.optionalString("snapshotId", MAX_SELECTOR_LENGTH)
            val versionId = payload.optionalString("versionId", MAX_SELECTOR_LENGTH)
            require((snapshotId == null) != (versionId == null)) {
                "Exactly one immutable snapshot or version selector is required"
            }
            val displayName = sanitizeDisplayName(
                payload.optionalString("fileName", MAX_REMOTE_PATH_LENGTH)
                    ?: remotePath.substringAfterLast('/'),
            )
            val mimeType = normalizeMimeType(payload.optionalString("mimeType", MAX_MIME_LENGTH))
            val sizeBytes = payload.optionalNonNegativeLong("sizeBytes")

            return OriginalShareRequest(
                requestId = requestId,
                remotePath = remotePath,
                snapshotId = snapshotId,
                versionId = versionId,
                displayName = displayName,
                mimeType = mimeType,
                sizeBytes = sizeBytes,
            )
        }
    }
}

internal data class OriginalShareCapability(
    val token: String,
    val remotePath: String,
    val snapshotId: String?,
    val versionId: String?,
    val displayName: String,
    val mimeType: String,
    val sizeBytes: Long?,
    val createdAtUnixMs: Long,
    val expiresAtUnixMs: Long,
) {
    fun toJson(): String = JSONObject()
        .put("remotePath", remotePath)
        .put("snapshotId", snapshotId)
        .put("versionId", versionId)
        .put("displayName", displayName)
        .put("mimeType", mimeType)
        .put("sizeBytes", sizeBytes)
        .put("createdAtUnixMs", createdAtUnixMs)
        .put("expiresAtUnixMs", expiresAtUnixMs)
        .toString()

    companion object {
        fun fromJson(token: String, raw: String): OriginalShareCapability {
            val value = JSONObject(raw)
            val remotePath = value.getString("remotePath")
            require(
                remotePath.isNotBlank() &&
                    remotePath.length <= MAX_REMOTE_PATH_LENGTH &&
                    '\u0000' !in remotePath,
            ) {
                "Stored share path is invalid"
            }
            return OriginalShareCapability(
                token = token,
                remotePath = remotePath,
                snapshotId = value.optionalString("snapshotId", MAX_SELECTOR_LENGTH),
                versionId = value.optionalString("versionId", MAX_SELECTOR_LENGTH),
                displayName = sanitizeDisplayName(value.getString("displayName")),
                mimeType = normalizeMimeType(value.optString("mimeType")),
                sizeBytes = value.optionalNonNegativeLong("sizeBytes"),
                createdAtUnixMs = value.getLong("createdAtUnixMs"),
                expiresAtUnixMs = value.getLong("expiresAtUnixMs"),
            ).also { capability ->
                require((capability.snapshotId == null) != (capability.versionId == null)) {
                    "Stored share selector is invalid"
                }
            }
        }
    }
}

internal class OriginalShareCapabilityStore(
    context: Context,
    private val clock: () -> Long = System::currentTimeMillis,
    private val tokenFactory: () -> String = { UUID.randomUUID().toString() },
) {
    private val preferences = context.applicationContext.getSharedPreferences(
        PREFERENCES_NAME,
        Context.MODE_PRIVATE,
    )

    fun create(request: OriginalShareRequest): OriginalShareCapability = synchronized(lock) {
        val now = clock()
        pruneLocked(now, entriesToReserve = 1)
        val token = tokenFactory().also {
            require(TOKEN_PATTERN.matches(it)) { "Invalid share token" }
        }
        val capability = OriginalShareCapability(
            token = token,
            remotePath = request.remotePath,
            snapshotId = request.snapshotId,
            versionId = request.versionId,
            displayName = request.displayName,
            mimeType = request.mimeType,
            sizeBytes = request.sizeBytes,
            createdAtUnixMs = now,
            expiresAtUnixMs = Math.addExact(now, CAPABILITY_LIFETIME_MS),
        )
        check(preferences.edit().putString(token, capability.toJson()).commit()) {
            "Could not persist the Android share capability"
        }
        capability
    }

    fun resolve(token: String): OriginalShareCapability = synchronized(lock) {
        if (!TOKEN_PATTERN.matches(token)) {
            throw FileNotFoundException("Invalid original-share capability")
        }
        val raw = preferences.getString(token, null)
            ?: throw FileNotFoundException("Original-share capability is unavailable")
        val capability = runCatching { OriginalShareCapability.fromJson(token, raw) }
            .getOrElse { error ->
                preferences.edit().remove(token).apply()
                throw FileNotFoundException("Original-share capability is invalid").apply {
                    initCause(error)
                }
            }
        if (capability.expiresAtUnixMs <= clock()) {
            preferences.edit().remove(token).apply()
            throw FileNotFoundException("Original-share capability has expired")
        }
        capability
    }

    fun remove(token: String) = synchronized(lock) {
        preferences.edit().remove(token).apply()
    }

    internal fun clearForTesting() = synchronized(lock) {
        preferences.edit().clear().commit()
    }

    private fun pruneLocked(now: Long, entriesToReserve: Int) {
        val active = mutableListOf<OriginalShareCapability>()
        val editor = preferences.edit()
        preferences.all.forEach { (token, raw) ->
            val capability = (raw as? String)?.let {
                runCatching { OriginalShareCapability.fromJson(token, it) }.getOrNull()
            }
            if (capability == null || capability.expiresAtUnixMs <= now) {
                editor.remove(token)
            } else {
                active += capability
            }
        }
        active
            .sortedByDescending(OriginalShareCapability::createdAtUnixMs)
            .drop(MAX_CAPABILITY_COUNT - entriesToReserve)
            .forEach { editor.remove(it.token) }
        check(editor.commit()) { "Could not prune Android share capabilities" }
    }

    private companion object {
        const val PREFERENCES_NAME = "ironmesh_share_capabilities"
        const val CAPABILITY_LIFETIME_MS = 24L * 60L * 60L * 1_000L
        const val MAX_CAPABILITY_COUNT = 64
        val TOKEN_PATTERN = Regex(
            "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-" +
                "[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
        )
        val lock = Any()
    }
}

internal class OriginalShareCoordinator(
    private val context: Context,
    private val capabilityStore: OriginalShareCapabilityStore = OriginalShareCapabilityStore(context),
) {
    fun handleWebMessage(rawMessage: String): String {
        if (rawMessage.length > MAX_WEB_MESSAGE_LENGTH) {
            return shareResponse("", "error", "Android share request is too large")
        }
        val requestId = requestIdForErrorResponse(rawMessage)
        return runCatching {
            val request = OriginalShareRequest.fromWebMessage(rawMessage)
            val capability = capabilityStore.create(request)
            val uri = originalShareDocumentUri(context, capability.token)
            try {
                val chooser = Intent.createChooser(
                    buildOriginalShareIntent(context, capability, uri),
                    "Share ${capability.displayName}",
                )
                if (context !is Activity) {
                    chooser.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                }
                context.startActivity(chooser)
            } catch (error: Exception) {
                capabilityStore.remove(capability.token)
                throw error
            }
            shareResponse(request.requestId, "opened")
        }.getOrElse { error ->
            shareResponse(
                requestId,
                "error",
                error.message?.take(240) ?: "Android could not open the share sheet.",
            )
        }
    }
}

internal fun originalShareDocumentId(token: String): String = "share:$token"

internal fun originalShareDocumentUri(context: Context, token: String): Uri =
    DocumentsContract.buildDocumentUri(
        "${context.packageName}.documents",
        originalShareDocumentId(token),
    )

internal fun buildOriginalShareIntent(
    context: Context,
    capability: OriginalShareCapability,
    uri: Uri,
): Intent = Intent(Intent.ACTION_SEND).apply {
    type = capability.mimeType
    putExtra(Intent.EXTRA_STREAM, uri)
    putExtra(Intent.EXTRA_TITLE, capability.displayName)
    clipData = ClipData.newUri(context.contentResolver, capability.displayName, uri)
    addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
}

private fun shareResponse(requestId: String, status: String, message: String? = null): String =
    JSONObject()
        .put("requestId", requestId)
        .put("status", status)
        .apply { if (message != null) put("message", message) }
        .toString()

private fun requestIdForErrorResponse(rawMessage: String): String = runCatching {
    JSONObject(rawMessage).optString("requestId").take(MAX_REQUEST_ID_LENGTH)
}.getOrDefault("")

private fun JSONObject.requiredString(name: String, maxLength: Int): String =
    optionalString(name, maxLength) ?: throw IllegalArgumentException("Missing $name")

private fun JSONObject.optionalString(name: String, maxLength: Int): String? {
    if (!has(name) || isNull(name)) {
        return null
    }
    val value = getString(name).trim()
    require(value.length <= maxLength) { "$name is too long" }
    require('\u0000' !in value) { "$name contains invalid characters" }
    return value.takeIf { it.isNotBlank() }
}

private fun JSONObject.optionalNonNegativeLong(name: String): Long? {
    if (!has(name) || isNull(name)) {
        return null
    }
    return getLong(name).also { require(it >= 0) { "$name must not be negative" } }
}

private fun sanitizeDisplayName(candidate: String): String {
    val leafName = candidate.substringAfterLast('/').substringAfterLast('\\').trim()
    val sanitized = buildString(leafName.length) {
        leafName.forEach { character ->
            append(if (character.code < 32 || character in "<>:\"/\\|?*") '_' else character)
        }
    }.trim().trim('.').take(MAX_DISPLAY_NAME_LENGTH)
    return sanitized.takeIf { it.isNotBlank() } ?: "original.bin"
}

private fun normalizeMimeType(candidate: String?): String {
    val normalized = candidate?.trim()?.lowercase()
    return normalized?.takeIf { MIME_TYPE_PATTERN.matches(it) } ?: "application/octet-stream"
}

private const val MAX_REQUEST_ID_LENGTH = 128
private const val MAX_WEB_MESSAGE_LENGTH = 32 * 1024
private const val MAX_REMOTE_PATH_LENGTH = 4_096
private const val MAX_SELECTOR_LENGTH = 512
private const val MAX_DISPLAY_NAME_LENGTH = 255
private const val MAX_MIME_LENGTH = 160
private val MIME_TYPE_PATTERN = Regex("^[a-z0-9][a-z0-9!#$&^_.+-]*/[a-z0-9][a-z0-9!#$&^_.+-]*$")
