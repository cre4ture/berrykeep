package io.ironmesh.android.data

import java.io.StringWriter
import java.io.Writer
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
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
    fun totalByteLimitEvictsOldestCompleteEntries() {
        val first = renderedEntry(1_000, "first")
        val second = renderedEntry(2_000, "second")
        val third = renderedEntry(3_000, "third")
        val retainedByteLimit = utf8Bytes(second + third)
        val buffer = AndroidDiagnosticLogBuffer(
            maxEntries = 10,
            maxTotalBytes = retainedByteLimit,
            maxEntryBytes = 1_024,
        )

        buffer.push(AndroidDiagnosticLogEntry(1_000, "INFO", "App", "first"))
        buffer.push(AndroidDiagnosticLogEntry(2_000, "INFO", "App", "second"))
        buffer.push(AndroidDiagnosticLogEntry(3_000, "INFO", "App", "third"))

        assertEquals(second + third, buffer.renderText())
        assertTrue(utf8Bytes(buffer.renderText()) <= retainedByteLimit)
        assertFalse(buffer.renderText().contains(first))
    }

    @Test
    fun oversizedEntriesAndStackTracesAreBounded() {
        val entryByteLimit = 96
        val detail = boundedDiagnosticMessage(
            message = "request failed",
            error = IllegalStateException("x".repeat(4_096)),
            maxBytes = entryByteLimit,
        )
        val buffer = AndroidDiagnosticLogBuffer(
            maxEntries = 10,
            maxTotalBytes = 1_024,
            maxEntryBytes = entryByteLimit,
        )

        buffer.push(AndroidDiagnosticLogEntry(1_000, "ERROR", "Network", detail))

        assertTrue(utf8Bytes(detail) <= entryByteLimit)
        assertTrue(detail.contains("[truncated]"))
        assertTrue(utf8Bytes(buffer.renderText()) <= entryByteLimit)
        assertTrue(buffer.renderText().contains("[truncated]"))
    }

    @Test
    fun unicodeTruncationPreservesCodePointBoundariesAndExactByteLimit() {
        val entryByteLimit = 57
        val buffer = AndroidDiagnosticLogBuffer(
            maxEntries = 10,
            maxTotalBytes = 1_024,
            maxEntryBytes = entryByteLimit,
        )

        buffer.push(AndroidDiagnosticLogEntry(1_000, "INFO", "Unicode", "🙂".repeat(100)))

        val rendered = buffer.renderText()
        assertTrue(utf8Bytes(rendered) <= entryByteLimit)
        assertTrue(rendered.contains("[truncated]"))
        assertFalse(rendered.contains('\uFFFD'))
        assertFalse(rendered.dropLast(1).last().isHighSurrogate())
    }

    @Test
    fun unicodePairSplitAcrossWriterCallsRemainsIntact() {
        val writer = Utf8TruncatingWriter(maxBytes = 4)

        writer.write("\uD83D")
        writer.write("\uDE42")

        assertEquals("🙂", writer.result())
        assertEquals(4, utf8Bytes(writer.result()))
    }

    @Test
    fun emptyExportIncludesBothEmptySectionMessages() {
        val writer = StringWriter()

        writeAndroidDiagnosticLogExport(
            writer = writer,
            generatedAtUnixMs = 1_000,
            metadata = emptyList(),
            applicationLogWriter = {},
            rustLog = "",
        )

        assertTrue(writer.toString().contains("(no Android application entries retained)"))
        assertTrue(writer.toString().contains("(no Rust tracing entries retained)"))
    }

    @Test
    fun exportStreamsApplicationEntriesIntoProvidedWriter() {
        val writer = RecordingWriter()

        writeAndroidDiagnosticLogExport(
            writer = writer,
            generatedAtUnixMs = 1_000,
            metadata = listOf("App version" to "1.2.3\nbranch"),
            applicationLogWriter = { applicationWriter ->
                applicationWriter.write("app entry one\n")
                applicationWriter.write("app entry two\n")
            },
            rustLog = "rust entry\n",
        )

        assertTrue(writer.writeCalls > 2)
        assertTrue(writer.content.contains("App version: 1.2.3 branch"))
        assertTrue(writer.content.contains("=== Android application log ===\napp entry one\napp entry two"))
        assertTrue(writer.content.contains("=== Rust tracing log ===\nrust entry"))
    }

    private fun renderedEntry(timestampUnixMs: Long, message: String): String {
        val buffer = AndroidDiagnosticLogBuffer(maxEntries = 1)
        buffer.push(AndroidDiagnosticLogEntry(timestampUnixMs, "INFO", "App", message))
        return buffer.renderText()
    }

    private fun utf8Bytes(value: String): Int = value.toByteArray(Charsets.UTF_8).size
}

private class RecordingWriter : Writer() {
    private val destination = StringBuilder()

    var writeCalls: Int = 0
        private set

    val content: String
        get() = destination.toString()

    override fun write(characters: CharArray, offset: Int, length: Int) {
        writeCalls += 1
        destination.append(characters, offset, length)
    }

    override fun write(value: String, offset: Int, length: Int) {
        writeCalls += 1
        destination.append(value, offset, offset + length)
    }

    override fun flush() = Unit

    override fun close() = Unit
}
