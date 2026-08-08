package io.ironmesh.android.data

import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AppConnectionStatusDiagnosticsTest {
    @Test
    fun transitionDiagnosticExplainsStatusRelevantFailure() {
        val previous = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            updatedUnixMs = 2_000L,
            lastSuccessfulConnectionUnixMs = 2_000L,
        )
        val failure = AppFailedConnectionAttempt(
            sourceLabel = "gallery map",
            impact = APP_CONNECTION_DIAGNOSTIC_IMPACT_USER_FACING,
            endpointLocator = "iroh://candidate",
            pathKind = "direct_quic",
            startedUnixMs = 2_900L,
            finishedUnixMs = 3_000L,
            method = "GET",
            url = "iroh://candidate/api/v1/store/index?media_filter=image",
            timeoutMs = 10_000L,
            error = "opening direct session failed\nrequest timed out",
        )
        val update = AppConnectionDiagnosticsUpdate(
            sourceLabel = "publisher snapshot",
            impact = APP_CONNECTION_DIAGNOSTIC_IMPACT_BACKGROUND_MAINTENANCE,
            failedAttempts = listOf(failure),
        )
        val next = RustPreferencesBridge.mergeAppConnectionDiagnostics(previous, update)

        val diagnostic = requireNotNull(
            buildAppConnectionStatusTransitionDiagnostic(
                previous = previous,
                next = next,
                update = update,
            ),
        )

        assertTrue(diagnostic.contains("previous_state=connected new_state=error"))
        assertTrue(diagnostic.contains("reason=latest_status_relevant_failure"))
        assertTrue(diagnostic.contains("update_impact=background_maintenance"))
        assertTrue(diagnostic.contains("update_source_label=\"publisher snapshot\""))
        assertTrue(diagnostic.contains("update_status_relevant_failure_count=1"))
        assertTrue(diagnostic.contains("latest_failure_impact=user_facing"))
        assertTrue(diagnostic.contains("latest_failure_path_kind=direct_quic"))
        assertTrue(diagnostic.contains("latest_failure_method=GET"))
        assertTrue(diagnostic.contains("latest_failure_timeout_ms=10000"))
        assertTrue(
            diagnostic.contains(
                "latest_failure_error=\"opening direct session failed request timed out\"",
            ),
        )
    }

    @Test
    fun transitionDiagnosticExplainsConnectionSuccess() {
        val previous = AppConnectionStatus(
            state = APP_CONNECTION_STATE_ERROR,
            updatedUnixMs = 3_000L,
            failedAttempts = listOf(
                AppFailedConnectionAttempt(
                    startedUnixMs = 2_900L,
                    finishedUnixMs = 3_000L,
                    method = "GET",
                    url = "iroh://failed/api/v1/store/index",
                    error = "request timed out",
                ),
            ),
        )
        val probeUrl = "iroh://healthy/api/v1/diagnostics/latency?response_bytes=0"
        val update = AppConnectionDiagnosticsUpdate(
            sourceLabel = "title latency monitor",
            lastSuccessfulConnectionUnixMs = 4_000L,
            lastSuccessfulConnectionUrl = probeUrl,
        )
        val next = RustPreferencesBridge.mergeAppConnectionDiagnostics(previous, update)

        val diagnostic = requireNotNull(
            buildAppConnectionStatusTransitionDiagnostic(
                previous = previous,
                next = next,
                update = update,
            ),
        )

        assertTrue(diagnostic.contains("previous_state=error new_state=connected"))
        assertTrue(diagnostic.contains("reason=latest_connection_success"))
        assertTrue(diagnostic.contains("update_has_connection_success=true"))
        assertTrue(diagnostic.contains("update_has_functional_success=false"))
        assertTrue(diagnostic.contains("update_last_success_unix_ms=4000"))
        assertTrue(diagnostic.contains("update_last_success_url=\"$probeUrl\""))
        assertTrue(diagnostic.contains("persisted_last_functional_success_unix_ms=null"))
    }

    @Test
    fun transitionDiagnosticIgnoresUpdatesThatKeepTheSameState() {
        val status = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            updatedUnixMs = 2_000L,
            lastSuccessfulConnectionUnixMs = 2_000L,
        )

        assertNull(
            buildAppConnectionStatusTransitionDiagnostic(
                previous = status,
                next = status.copy(updatedUnixMs = 3_000L),
                update = AppConnectionDiagnosticsUpdate(
                    lastSuccessfulConnectionUnixMs = 3_000L,
                ),
            ),
        )
    }
}
