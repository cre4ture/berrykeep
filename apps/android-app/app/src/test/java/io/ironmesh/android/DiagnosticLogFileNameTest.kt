package io.ironmesh.android

import java.time.Instant
import org.junit.Assert.assertEquals
import org.junit.Test

class DiagnosticLogFileNameTest {
    @Test
    fun filenameUsesStableUtcTimestamp() {
        assertEquals(
            "berrykeep-diagnostic-20260728-123456.log",
            diagnosticLogFileName(Instant.parse("2026-07-28T12:34:56Z")),
        )
    }
}
