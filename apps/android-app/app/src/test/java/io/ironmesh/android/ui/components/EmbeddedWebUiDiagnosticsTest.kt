package io.ironmesh.android.ui.components

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class EmbeddedWebUiDiagnosticsTest {
    @Test
    fun httpFailureKeepsTheApiPathButRedactsQueryValuesAndSecrets() {
        val rendered = EmbeddedWebUiDiagnostic(
            level = EmbeddedWebUiDiagnosticLevel.WARN,
            event = "http_error",
            pageUrl = "http://127.0.0.1:40000/gallery?embedded=gallery_map",
            requestUrl = "http://127.0.0.1:40000/api/v1/maps/config?session=private-value",
            method = "get",
            statusCode = 503,
            detail = "Service unavailable; token=another-private-value",
        ).render()

        assertEquals(
            "event=http_error page_path=/gallery request_path=/api/v1/maps/config method=GET status=503 detail=Service unavailable; token=<redacted>",
            rendered,
        )
        assertFalse(rendered.contains("private-value"))
        assertTrue(isEmbeddedWebUiApiRequest("http://127.0.0.1:40000/api/v1/maps/config?token=x"))
    }

    @Test
    fun javascriptDiagnosticRedactsUrlsAndBoundsUntrustedConsoleText() {
        val detail = "Authorization: bearer-value; Bearer direct-bearer-value at https://example.test/api/v1/maps/config?token=private"
        val rendered = EmbeddedWebUiDiagnostic(
            level = EmbeddedWebUiDiagnosticLevel.ERROR,
            event = "javascript_console",
            scriptUrl = "http://127.0.0.1:40000/assets/app.js?build=private",
            lineNumber = 42,
            detail = detail,
        ).render()

        assertTrue(rendered.contains("script_path=/assets/app.js"))
        assertTrue(rendered.contains("Authorization: <redacted>"))
        assertTrue(rendered.contains("Bearer <redacted>"))
        assertTrue(rendered.contains("/api/v1/maps/config"))
        assertFalse(rendered.contains("bearer-value"))
        assertFalse(rendered.contains("direct-bearer-value"))
        assertFalse(rendered.contains("token=private"))
    }

    @Test
    fun javascriptDiagnosticRedactsCompoundAndJsonSecretKeys() {
        val rendered = EmbeddedWebUiDiagnostic(
            level = EmbeddedWebUiDiagnosticLevel.ERROR,
            event = "javascript_console",
            detail = "sessionToken=session-value authToken=auth-value access_token=access-value " +
                "api_secret=secret-value apiKey=key-value \"token\":\"json-token\" " +
                "\"apiSecret\": \"json-secret\"",
        ).render()

        assertTrue(rendered.contains("sessionToken=<redacted>"))
        assertTrue(rendered.contains("access_token=<redacted>"))
        assertTrue(rendered.contains("apiKey=<redacted>"))
        assertTrue(rendered.contains("'token':<redacted>"))
        assertTrue(rendered.contains("'apiSecret': <redacted>"))
        assertFalse(rendered.contains("session-value"))
        assertFalse(rendered.contains("auth-value"))
        assertFalse(rendered.contains("access-value"))
        assertFalse(rendered.contains("secret-value"))
        assertFalse(rendered.contains("key-value"))
        assertFalse(rendered.contains("json-token"))
        assertFalse(rendered.contains("json-secret"))
    }

    @Test
    fun reporterSuppressesShortBurstOfIdenticalFailures() {
        var now = 1_000L
        val emitted = mutableListOf<String>()
        val reporter = EmbeddedWebUiDiagnosticReporter(
            nowUnixMs = { now },
            emit = { _, message -> emitted += message },
        )
        val failure = EmbeddedWebUiDiagnostic(
            level = EmbeddedWebUiDiagnosticLevel.WARN,
            event = "http_error",
            requestUrl = "http://127.0.0.1:40000/api/v1/maps/config",
            statusCode = 503,
        )

        reporter.report(failure)
        now += 5_000
        reporter.report(failure)
        now += 5_000
        reporter.report(failure)

        assertEquals(2, emitted.size)
    }
}
