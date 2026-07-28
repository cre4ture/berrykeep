package io.ironmesh.android.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class AndroidDiagnosticLogTest {
    @Test
    fun bufferRetainsNewestEntriesInChronologicalOrder() {
        val buffer = AndroidDiagnosticLogBuffer(maxEntries = 2)
        buffer.push(AndroidDiagnosticLogEntry(1_000, "INFO", "App", "first"))
        buffer.push(AndroidDiagnosticLogEntry(2_000, "WARN", "App", "second"))
        buffer.push(AndroidDiagnosticLogEntry(3_000, "ERROR", "Rust", "third"))

        assertEquals(
            """
            1970-01-01T00:00:02Z WARN/App: second
            1970-01-01T00:00:03Z ERROR/Rust: third

            """.trimIndent(),
            buffer.renderText(),
        )
    }

    @Test
    fun exportSeparatesApplicationAndRustLogs() {
        val export = buildAndroidDiagnosticLogExport(
            generatedAtUnixMs = 1_000,
            metadata = listOf("App version" to "1.2.3"),
            applicationLog = "app entry\n",
            rustLog = "rust entry\n",
        )

        assertTrue(export.contains("App version: 1.2.3"))
        assertTrue(export.contains("=== Android application log ===\napp entry"))
        assertTrue(export.contains("=== Rust tracing log ===\nrust entry"))
    }
}
