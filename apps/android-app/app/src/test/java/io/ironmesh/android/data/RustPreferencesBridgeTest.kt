package io.ironmesh.android.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class RustPreferencesBridgeTest {
    @Test
    fun functionalSuccessSurvivesLaterConnectivityProbe() {
        val functionalUrl = "https://example.test/api/v1/store/index?media_filter=image"
        val probeUrl =
            "https://example.test/api/v1/diagnostics/latency?response_bytes=0"
        val failure = AppFailedConnectionAttempt(
            startedUnixMs = 900L,
            finishedUnixMs = 1_000L,
            method = "GET",
            url = functionalUrl,
            error = "request timed out",
        )

        val afterFailure = RustPreferencesBridge.mergeAppConnectionDiagnostics(
            current = AppConnectionStatus(),
            update = AppConnectionDiagnosticsUpdate(failedAttempts = listOf(failure)),
        )
        assertEquals(APP_CONNECTION_STATE_ERROR, afterFailure.state)
        assertNull(afterFailure.lastSuccessfulFunctionalRequestUnixMs)

        val afterFunctionalSuccess = RustPreferencesBridge.mergeAppConnectionDiagnostics(
            current = afterFailure,
            update = AppConnectionDiagnosticsUpdate(
                lastSuccessfulConnectionUnixMs = 2_000L,
                lastSuccessfulConnectionUrl = functionalUrl,
                lastSuccessfulFunctionalRequestUnixMs = 2_000L,
                lastSuccessfulFunctionalRequestUrl = functionalUrl,
            ),
        )
        assertEquals(APP_CONNECTION_STATE_CONNECTED, afterFunctionalSuccess.state)

        val afterProbe = RustPreferencesBridge.mergeAppConnectionDiagnostics(
            current = afterFunctionalSuccess,
            update = AppConnectionDiagnosticsUpdate(
                lastSuccessfulConnectionUnixMs = 3_000L,
                lastSuccessfulConnectionUrl = probeUrl,
            ),
        )

        assertEquals(3_000L, afterProbe.lastSuccessfulConnectionUnixMs)
        assertEquals(probeUrl, afterProbe.lastSuccessfulConnectionUrl)
        assertEquals(2_000L, afterProbe.lastSuccessfulFunctionalRequestUnixMs)
        assertEquals(functionalUrl, afterProbe.lastSuccessfulFunctionalRequestUrl)
        assertEquals(listOf(failure), afterProbe.failedAttempts)
    }

    @Test
    fun backgroundMaintenanceFailuresAreRetainedWithoutChangingHomeConnectionState() {
        val functionalUrl = "https://example.test/api/v1/store/index?media_filter=image"
        val current = AppConnectionStatus(
            state = APP_CONNECTION_STATE_CONNECTED,
            message = "Last request succeeded via $functionalUrl",
            updatedUnixMs = 2_000L,
            lastSuccessfulConnectionUnixMs = 2_000L,
            lastSuccessfulConnectionUrl = functionalUrl,
            lastSuccessfulFunctionalRequestUnixMs = 2_000L,
            lastSuccessfulFunctionalRequestUrl = functionalUrl,
        )
        val backgroundFailure = AppFailedConnectionAttempt(
            startedUnixMs = 2_900L,
            finishedUnixMs = 3_000L,
            method = "GET",
            url = "iroh://candidate/api/v1/cluster/status",
            error = "candidate timed out",
        )

        val afterBackgroundFailure = RustPreferencesBridge.mergeAppConnectionDiagnostics(
            current = current,
            update = AppConnectionDiagnosticsUpdate(
                impact = APP_CONNECTION_DIAGNOSTIC_IMPACT_BACKGROUND_MAINTENANCE,
                lastSuccessfulConnectionUnixMs = 3_100L,
                lastSuccessfulConnectionUrl = "iroh://candidate/api/v1/diagnostics/latency",
                failedAttempts = listOf(backgroundFailure),
            ),
        )

        assertEquals(APP_CONNECTION_STATE_CONNECTED, afterBackgroundFailure.state)
        assertEquals(current.message, afterBackgroundFailure.message)
        assertEquals(2_000L, afterBackgroundFailure.updatedUnixMs)
        assertEquals(2_000L, afterBackgroundFailure.lastSuccessfulConnectionUnixMs)
        assertEquals(1, afterBackgroundFailure.failedAttempts.size)
        assertEquals(
            APP_CONNECTION_DIAGNOSTIC_IMPACT_BACKGROUND_MAINTENANCE,
            afterBackgroundFailure.failedAttempts.single().impact,
        )

        val afterFunctionalFailure = RustPreferencesBridge.mergeAppConnectionDiagnostics(
            current = afterBackgroundFailure,
            update = AppConnectionDiagnosticsUpdate(
                impact = APP_CONNECTION_DIAGNOSTIC_IMPACT_USER_FACING,
                failedAttempts = listOf(
                    AppFailedConnectionAttempt(
                        startedUnixMs = 3_900L,
                        finishedUnixMs = 4_000L,
                        method = "GET",
                        url = functionalUrl,
                        error = "request timed out",
                    ),
                ),
            ),
        )

        assertEquals(APP_CONNECTION_STATE_ERROR, afterFunctionalFailure.state)
        assertEquals(4_000L, afterFunctionalFailure.updatedUnixMs)
    }

    @Test
    fun backgroundFailuresCannotEvictAnUnresolvedFunctionalFailure() {
        val functionalUrl = "https://example.test/api/v1/store/index?media_filter=image"
        val afterFunctionalFailure = RustPreferencesBridge.mergeAppConnectionDiagnostics(
            current = AppConnectionStatus(),
            update = AppConnectionDiagnosticsUpdate(
                failedAttempts = listOf(
                    AppFailedConnectionAttempt(
                        startedUnixMs = 900L,
                        finishedUnixMs = 1_000L,
                        method = "GET",
                        url = functionalUrl,
                        error = "request timed out",
                    ),
                ),
            ),
        )

        val backgroundFailures = (1L..20L).map { index ->
            AppFailedConnectionAttempt(
                startedUnixMs = 2_000L + index,
                method = "GET",
                url = "iroh://candidate-$index/api/v1/cluster/status",
                error = "candidate timed out",
            )
        }
        val afterBackgroundBurst = RustPreferencesBridge.mergeAppConnectionDiagnostics(
            current = afterFunctionalFailure,
            update = AppConnectionDiagnosticsUpdate(
                impact = APP_CONNECTION_DIAGNOSTIC_IMPACT_BACKGROUND_MAINTENANCE,
                failedAttempts = backgroundFailures,
            ),
        )

        assertEquals(APP_CONNECTION_STATE_ERROR, afterBackgroundBurst.state)
        assertEquals(1_000L, afterBackgroundBurst.updatedUnixMs)
        assertEquals(
            functionalUrl,
            afterBackgroundBurst.failedAttempts
                .single { attempt -> attempt.impact == APP_CONNECTION_DIAGNOSTIC_IMPACT_USER_FACING }
                .url,
        )
        assertEquals(
            12,
            afterBackgroundBurst.failedAttempts.count { attempt ->
                attempt.impact == APP_CONNECTION_DIAGNOSTIC_IMPACT_BACKGROUND_MAINTENANCE
            },
        )
    }
}
