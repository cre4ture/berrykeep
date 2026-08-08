package io.ironmesh.android.data

private const val APP_CONNECTION_DIAGNOSTIC_TAG = "AppConnectionStatus"
private const val MAX_DIAGNOSTIC_FIELD_LENGTH = 512

internal fun logPersistedAppConnectionStatusTransition(
    previous: AppConnectionStatus,
    next: AppConnectionStatus,
    update: AppConnectionDiagnosticsUpdate,
) {
    val diagnostic = buildAppConnectionStatusTransitionDiagnostic(
        previous = previous,
        next = next,
        update = update,
    ) ?: return

    if (next.state == APP_CONNECTION_STATE_ERROR) {
        AndroidDiagnosticLog.w(APP_CONNECTION_DIAGNOSTIC_TAG, diagnostic)
    } else {
        AndroidDiagnosticLog.i(APP_CONNECTION_DIAGNOSTIC_TAG, diagnostic)
    }
}

internal fun buildAppConnectionStatusTransitionDiagnostic(
    previous: AppConnectionStatus,
    next: AppConnectionStatus,
    update: AppConnectionDiagnosticsUpdate,
): String? {
    if (previous.state == next.state) {
        return null
    }

    val latestFailure = latestStatusRelevantFailure(next.failedAttempts)
    val latestFailureUnixMs = latestFailure?.finishedUnixMs ?: latestFailure?.startedUnixMs
    val latestSuccessUnixMs = next.lastSuccessfulConnectionUnixMs
    val statusRelevantUpdateFailures = update.failedAttempts.count { attempt ->
        attempt.affectsAppConnectionStatus()
    }

    return buildString {
        append("event=app_connection_status_transition")
        appendDiagnosticToken("previous_state", previous.state)
        appendDiagnosticToken("new_state", next.state)
        appendDiagnosticToken(
            "reason",
            appConnectionTransitionReason(next.state, latestSuccessUnixMs, latestFailureUnixMs),
        )
        appendDiagnosticNumber("previous_updated_unix_ms", previous.updatedUnixMs)
        appendDiagnosticNumber("status_updated_unix_ms", next.updatedUnixMs)
        appendDiagnosticToken("update_impact", update.impact)
        appendDiagnosticText("update_source_label", update.sourceLabel)
        appendDiagnosticBoolean(
            "update_has_connection_success",
            update.lastSuccessfulConnectionUnixMs != null,
        )
        appendDiagnosticBoolean(
            "update_has_functional_success",
            update.lastSuccessfulFunctionalRequestUnixMs != null,
        )
        appendDiagnosticNumber("update_failed_attempt_count", update.failedAttempts.size.toLong())
        appendDiagnosticNumber(
            "update_status_relevant_failure_count",
            statusRelevantUpdateFailures.toLong(),
        )
        appendDiagnosticNumber(
            "update_background_failure_count",
            (update.failedAttempts.size - statusRelevantUpdateFailures).toLong(),
        )
        appendDiagnosticNumber(
            "update_last_success_unix_ms",
            update.lastSuccessfulConnectionUnixMs,
        )
        appendDiagnosticText("update_last_success_url", update.lastSuccessfulConnectionUrl)
        appendDiagnosticNumber(
            "update_last_functional_success_unix_ms",
            update.lastSuccessfulFunctionalRequestUnixMs,
        )
        appendDiagnosticText(
            "update_last_functional_success_url",
            update.lastSuccessfulFunctionalRequestUrl,
        )
        appendDiagnosticNumber("persisted_last_success_unix_ms", latestSuccessUnixMs)
        appendDiagnosticNumber(
            "persisted_last_functional_success_unix_ms",
            next.lastSuccessfulFunctionalRequestUnixMs,
        )
        appendDiagnosticNumber("latest_failure_unix_ms", latestFailureUnixMs)
        appendDiagnosticToken("latest_failure_impact", latestFailure?.impact)
        appendDiagnosticText("latest_failure_source_label", latestFailure?.sourceLabel)
        appendDiagnosticText("latest_failure_endpoint", latestFailure?.endpointLocator)
        appendDiagnosticToken("latest_failure_path_kind", latestFailure?.pathKind)
        appendDiagnosticToken("latest_failure_method", latestFailure?.method)
        appendDiagnosticText("latest_failure_url", latestFailure?.url)
        appendDiagnosticNumber("latest_failure_timeout_ms", latestFailure?.timeoutMs)
        appendDiagnosticText("latest_failure_error", latestFailure?.error)
    }
}

internal fun latestStatusRelevantFailure(
    failures: List<AppFailedConnectionAttempt>,
): AppFailedConnectionAttempt? = failures
    .asSequence()
    .filter { attempt -> attempt.affectsAppConnectionStatus() }
    .maxByOrNull { attempt -> attempt.finishedUnixMs ?: attempt.startedUnixMs }

private fun appConnectionTransitionReason(
    nextState: String,
    latestSuccessUnixMs: Long?,
    latestFailureUnixMs: Long?,
): String = when {
    latestSuccessUnixMs != null &&
        latestSuccessUnixMs >= (latestFailureUnixMs ?: Long.MIN_VALUE) &&
        nextState == APP_CONNECTION_STATE_CONNECTED ->
        "latest_connection_success"
    latestFailureUnixMs != null && nextState == APP_CONNECTION_STATE_ERROR ->
        "latest_status_relevant_failure"
    else -> "state_update"
}

private fun StringBuilder.appendDiagnosticToken(name: String, value: String?) {
    append(' ').append(name).append('=')
    if (value == null) {
        append("null")
    } else {
        append(
            value
                .trim()
                .take(MAX_DIAGNOSTIC_FIELD_LENGTH)
                .map { character ->
                    if (character.isLetterOrDigit() || character == '_' || character == '-') {
                        character
                    } else {
                        '_'
                    }
                }
                .joinToString(separator = ""),
        )
    }
}

private fun StringBuilder.appendDiagnosticText(name: String, value: String?) {
    append(' ').append(name).append('=')
    if (value == null) {
        append("null")
        return
    }

    val normalized = value
        .map { character -> if (character.isISOControl()) ' ' else character }
        .joinToString(separator = "")
        .trim()
        .take(MAX_DIAGNOSTIC_FIELD_LENGTH)
    append('"')
    normalized.forEach { character ->
        if (character == '"' || character == '\\') {
            append('\\')
        }
        append(character)
    }
    append('"')
}

private fun StringBuilder.appendDiagnosticNumber(name: String, value: Long?) {
    append(' ').append(name).append('=').append(value?.toString() ?: "null")
}

private fun StringBuilder.appendDiagnosticBoolean(name: String, value: Boolean) {
    append(' ').append(name).append('=').append(value)
}
