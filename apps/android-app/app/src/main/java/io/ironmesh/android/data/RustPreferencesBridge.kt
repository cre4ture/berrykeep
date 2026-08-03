package io.ironmesh.android.data

import android.content.Context
import com.squareup.moshi.Moshi
import com.squareup.moshi.kotlin.reflect.KotlinJsonAdapterFactory
import org.json.JSONObject

object RustPreferencesBridge {
    private const val MAX_USER_FACING_FAILED_CONNECTION_ATTEMPTS = 12
    private const val MAX_BACKGROUND_FAILED_CONNECTION_ATTEMPTS = 12

    @Volatile
    private var appContext: Context? = null

    private val diagnosticsUpdateAdapter by lazy {
        Moshi.Builder()
            .add(KotlinJsonAdapterFactory())
            .build()
            .adapter(AppConnectionDiagnosticsUpdate::class.java)
    }

    @JvmStatic
    fun initialize(context: Context) {
        appContext = context.applicationContext
    }

    @JvmStatic
    @Throws(DeviceIdentityStorageException::class)
    fun updateDeviceAuthBootstrapJson(bootstrapJson: String) {
        val context = appContext ?: error("RustPreferencesBridge is not initialized")
        val current = IronmeshPreferences.getDeviceAuthState(context)
        IronmeshPreferences.setDeviceAuthState(
            context,
            current.copy(connectionInput = bootstrapJson.trim()),
        )
    }

    @JvmStatic
    @Throws(DeviceIdentityStorageException::class)
    fun updateDeviceAuthClientIdentityJson(clientIdentityJson: String) {
        val context = appContext ?: error("RustPreferencesBridge is not initialized")
        val current = IronmeshPreferences.getDeviceAuthState(context)
        val json = JSONObject(clientIdentityJson)
        IronmeshPreferences.setDeviceAuthState(
            context,
            current.copy(
                clusterId = json.requiredTrimmedString("cluster_id"),
                deviceId = json.requiredTrimmedString("device_id"),
                label = json.optionalTrimmedString("label"),
                publicKeyPem = json.requiredTrimmedString("public_key_pem"),
                privateKeyPem = json.requiredTrimmedString("private_key_pem"),
                credentialPem = json.requiredTrimmedString("credential_pem"),
                rendezvousClientIdentityPem =
                    json.optionalTrimmedString("rendezvous_client_identity_pem"),
            ),
        )
    }

    @JvmStatic
    fun updateAppConnectionDiagnosticsJson(diagnosticsJson: String) {
        val context = appContext ?: error("RustPreferencesBridge is not initialized")
        val update = diagnosticsUpdateAdapter.fromJson(diagnosticsJson) ?: return
        val current = IronmeshPreferences.getAppConnectionStatus(context)
        IronmeshPreferences.setAppConnectionStatus(
            context,
            mergeAppConnectionDiagnostics(current, update),
        )
    }

    internal fun mergeAppConnectionDiagnostics(
        current: AppConnectionStatus,
        update: AppConnectionDiagnosticsUpdate,
    ): AppConnectionStatus {
        val scopedUpdateFailures = update.failedAttempts
        val updateCarriesUserFacingSuccess =
            update.lastSuccessfulConnectionUnixMs != null ||
                update.lastSuccessfulFunctionalRequestUnixMs != null
        val updateAffectsAppConnectionStatus =
            updateCarriesUserFacingSuccess ||
                scopedUpdateFailures.any { attempt -> attempt.affectsAppConnectionStatus() }
        val mergedFailures = retainRecentFailuresByImpact(
            current.failedAttempts + scopedUpdateFailures,
        )
        val statusRelevantFailures = mergedFailures.filter { attempt ->
            attempt.affectsAppConnectionStatus()
        }

        val effectiveLastSuccessUnixMs = when {
            !updateAffectsAppConnectionStatus -> current.lastSuccessfulConnectionUnixMs
            current.lastSuccessfulConnectionUnixMs == null -> update.lastSuccessfulConnectionUnixMs
            update.lastSuccessfulConnectionUnixMs == null -> current.lastSuccessfulConnectionUnixMs
            update.lastSuccessfulConnectionUnixMs >= current.lastSuccessfulConnectionUnixMs ->
                update.lastSuccessfulConnectionUnixMs
            else -> current.lastSuccessfulConnectionUnixMs
        }
        val effectiveLastSuccessUrl = when {
            effectiveLastSuccessUnixMs == null -> null
            updateAffectsAppConnectionStatus &&
                effectiveLastSuccessUnixMs == update.lastSuccessfulConnectionUnixMs ->
                update.lastSuccessfulConnectionUrl?.takeIf { it.isNotBlank() }
                    ?: current.lastSuccessfulConnectionUrl
            else -> current.lastSuccessfulConnectionUrl
        }
        val effectiveLastFunctionalSuccessUnixMs = when {
            !updateAffectsAppConnectionStatus -> current.lastSuccessfulFunctionalRequestUnixMs
            current.lastSuccessfulFunctionalRequestUnixMs == null ->
                update.lastSuccessfulFunctionalRequestUnixMs
            update.lastSuccessfulFunctionalRequestUnixMs == null ->
                current.lastSuccessfulFunctionalRequestUnixMs
            update.lastSuccessfulFunctionalRequestUnixMs >=
                current.lastSuccessfulFunctionalRequestUnixMs ->
                update.lastSuccessfulFunctionalRequestUnixMs
            else -> current.lastSuccessfulFunctionalRequestUnixMs
        }
        val effectiveLastFunctionalSuccessUrl = when {
            effectiveLastFunctionalSuccessUnixMs == null -> null
            updateAffectsAppConnectionStatus &&
                effectiveLastFunctionalSuccessUnixMs ==
                update.lastSuccessfulFunctionalRequestUnixMs ->
                update.lastSuccessfulFunctionalRequestUrl?.takeIf { it.isNotBlank() }
                    ?: current.lastSuccessfulFunctionalRequestUrl
            else -> current.lastSuccessfulFunctionalRequestUrl
        }

        val latestFailure = statusRelevantFailures.maxByOrNull { attempt ->
            attempt.finishedUnixMs ?: attempt.startedUnixMs
        }
        val latestFailureUnixMs = latestFailure?.finishedUnixMs ?: latestFailure?.startedUnixMs
        val latestSuccessUnixMs = effectiveLastSuccessUnixMs
        val latestEventUnixMs = listOfNotNull(latestSuccessUnixMs, latestFailureUnixMs).maxOrNull()

        val shouldRefreshState = updateAffectsAppConnectionStatus &&
            latestEventUnixMs != null && latestEventUnixMs >= current.updatedUnixMs
        val nextState = when {
            !shouldRefreshState -> current.state
            latestSuccessUnixMs != null &&
                latestSuccessUnixMs >= (latestFailureUnixMs ?: Long.MIN_VALUE) ->
                APP_CONNECTION_STATE_CONNECTED
            latestFailureUnixMs != null -> APP_CONNECTION_STATE_ERROR
            else -> current.state
        }
        val nextMessage = when {
            !shouldRefreshState -> current.message
            nextState == APP_CONNECTION_STATE_CONNECTED && !effectiveLastSuccessUrl.isNullOrBlank() ->
                "Last request succeeded via $effectiveLastSuccessUrl"
            nextState == APP_CONNECTION_STATE_CONNECTED ->
                "Last app request succeeded"
            nextState == APP_CONNECTION_STATE_ERROR ->
                latestFailure?.error?.takeIf { it.isNotBlank() }
                    ?: "Last app request failed"
            else -> current.message
        }

        return current.copy(
            state = nextState,
            message = nextMessage,
            updatedUnixMs = if (shouldRefreshState) {
                latestEventUnixMs ?: current.updatedUnixMs
            } else {
                current.updatedUnixMs
            },
            retryAttemptCount = if (shouldRefreshState) 0L else current.retryAttemptCount,
            nextRetryUnixMs = if (shouldRefreshState) null else current.nextRetryUnixMs,
            lastSuccessfulConnectionUnixMs = effectiveLastSuccessUnixMs,
            lastSuccessfulConnectionUrl = effectiveLastSuccessUrl,
            lastSuccessfulFunctionalRequestUnixMs = effectiveLastFunctionalSuccessUnixMs,
            lastSuccessfulFunctionalRequestUrl = effectiveLastFunctionalSuccessUrl,
            failedAttempts = mergedFailures,
        )
    }

    private fun retainRecentFailuresByImpact(
        failures: List<AppFailedConnectionAttempt>,
    ): List<AppFailedConnectionAttempt> {
        val distinctFailures = failures.distinctBy { attempt -> failedAttemptKey(attempt) }
        return listOf(
            distinctFailures
                .asSequence()
                .filter { attempt -> attempt.affectsAppConnectionStatus() }
                .sortedByDescending { attempt -> attempt.finishedUnixMs ?: attempt.startedUnixMs }
                .take(MAX_USER_FACING_FAILED_CONNECTION_ATTEMPTS)
                .toList(),
            distinctFailures
                .asSequence()
                .filterNot { attempt -> attempt.affectsAppConnectionStatus() }
                .sortedByDescending { attempt -> attempt.finishedUnixMs ?: attempt.startedUnixMs }
                .take(MAX_BACKGROUND_FAILED_CONNECTION_ATTEMPTS)
                .toList(),
        ).flatten()
            .sortedByDescending { attempt -> attempt.finishedUnixMs ?: attempt.startedUnixMs }
    }

    @JvmStatic
    fun cacheDirPath(): String {
        val context = appContext ?: error("RustPreferencesBridge is not initialized")
        return context.cacheDir.absolutePath
    }

    @JvmStatic
    fun noBackupFilesDirPath(): String {
        val context = appContext ?: error("RustPreferencesBridge is not initialized")
        return context.noBackupFilesDir.absolutePath
    }

    private fun JSONObject.optionalTrimmedString(name: String): String? {
        if (!has(name) || isNull(name)) {
            return null
        }
        return getString(name).trim().takeIf { it.isNotEmpty() }
    }

    private fun JSONObject.requiredTrimmedString(name: String): String =
        optionalTrimmedString(name) ?: throw DeviceIdentityStorageException(
            "The client identity update is missing $name. Clear local enrollment and enroll this device again.",
        )

    private fun failedAttemptKey(attempt: AppFailedConnectionAttempt): String {
        return listOf(
            attempt.sourceLabel.orEmpty(),
            attempt.impact,
            attempt.endpointLocator,
            attempt.pathKind,
            attempt.startedUnixMs.toString(),
            attempt.finishedUnixMs?.toString().orEmpty(),
            attempt.method,
            attempt.url,
            attempt.timeoutMs?.toString().orEmpty(),
            attempt.error.orEmpty(),
        ).joinToString("|")
    }
}
