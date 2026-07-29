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
}
