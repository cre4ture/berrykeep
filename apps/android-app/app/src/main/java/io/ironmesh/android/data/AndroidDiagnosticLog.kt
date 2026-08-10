package io.ironmesh.android.data

import java.io.PrintWriter
import java.io.StringWriter
import java.io.Writer
import java.time.Instant
import java.time.format.DateTimeFormatter
import java.util.ArrayDeque

internal data class AndroidDiagnosticLogEntry(
    val timestampUnixMs: Long,
    val level: String,
    val tag: String,
    val message: String,
)

private data class RetainedAndroidDiagnosticLogEntry(
    val renderedText: String,
    val utf8Bytes: Int,
)

internal class AndroidDiagnosticLogBuffer(
    maxEntries: Int,
    maxTotalBytes: Int = DEFAULT_MAX_TOTAL_BYTES,
    maxEntryBytes: Int = DEFAULT_MAX_ENTRY_BYTES,
) {
    private val maxEntries = maxEntries.coerceAtLeast(1)
    private val maxTotalBytes = maxTotalBytes.coerceAtLeast(1)
    private val maxEntryBytes = maxEntryBytes.coerceIn(1, this.maxTotalBytes)
    private val entries = ArrayDeque<RetainedAndroidDiagnosticLogEntry>()
    private var retainedUtf8Bytes = 0L

    @Synchronized
    fun push(entry: AndroidDiagnosticLogEntry) {
        val retainedEntry = retainEntry(entry)
        entries.addLast(retainedEntry)
        retainedUtf8Bytes += retainedEntry.utf8Bytes
        while (entries.size > maxEntries || retainedUtf8Bytes > maxTotalBytes) {
            retainedUtf8Bytes -= entries.removeFirst().utf8Bytes
        }
    }

    /**
     * Writes a stable capture-order snapshot without holding the buffer lock during I/O.
     * The snapshot only copies references to immutable, already-bounded strings.
     */
    fun writeTo(writer: Writer): Boolean {
        val snapshot = synchronized(this) { entries.toList() }
        snapshot.forEach { entry ->
            writer.write(entry.renderedText)
            writer.write("\n")
        }
        return snapshot.isNotEmpty()
    }

    internal fun renderText(): String = StringWriter().also(::writeTo).toString()

    private fun retainEntry(entry: AndroidDiagnosticLogEntry): RetainedAndroidDiagnosticLogEntry {
        val writer = Utf8TruncatingWriter(maxEntryBytes - NEWLINE_UTF8_BYTES)
        writer.write(formatDiagnosticTimestamp(entry.timestampUnixMs))
        writer.write(" ")
        writer.write(entry.level)
        writer.write("/")
        writer.write(entry.tag)
        writer.write(": ")
        writer.write(entry.message)
        val renderedText = writer.result()
        return RetainedAndroidDiagnosticLogEntry(
            renderedText = renderedText,
            utf8Bytes = utf8Length(renderedText) + NEWLINE_UTF8_BYTES,
        )
    }

    companion object {
        // Four MiB retains useful history while giving the app a predictable export-size ceiling.
        internal const val DEFAULT_MAX_TOTAL_BYTES = 4 * 1024 * 1024

        // A single exception must not consume the entire buffer; 64 KiB still carries long stack traces.
        internal const val DEFAULT_MAX_ENTRY_BYTES = 64 * 1024
        private const val NEWLINE_UTF8_BYTES = 1
    }
}

object AndroidDiagnosticLog {
    private const val MAX_ENTRIES = 10_000
    private val buffer = AndroidDiagnosticLogBuffer(MAX_ENTRIES)

    fun d(tag: String, message: String, error: Throwable? = null): Int =
        record("DEBUG", tag, message, error) { android.util.Log.d(tag, message, error) }

    fun i(tag: String, message: String, error: Throwable? = null): Int =
        record("INFO", tag, message, error) { android.util.Log.i(tag, message, error) }

    fun w(tag: String, message: String, error: Throwable? = null): Int =
        record("WARN", tag, message, error) { android.util.Log.w(tag, message, error) }

    fun e(tag: String, message: String, error: Throwable? = null): Int =
        record("ERROR", tag, message, error) { android.util.Log.e(tag, message, error) }

    fun writeTo(writer: Writer): Boolean = buffer.writeTo(writer)

    private fun record(
        level: String,
        tag: String,
        message: String,
        error: Throwable?,
        androidLog: () -> Int,
    ): Int {
        buffer.push(
            AndroidDiagnosticLogEntry(
                timestampUnixMs = System.currentTimeMillis(),
                level = level,
                tag = tag,
                message = boundedDiagnosticMessage(
                    message = message,
                    error = error,
                    maxBytes = AndroidDiagnosticLogBuffer.DEFAULT_MAX_ENTRY_BYTES,
                ),
            ),
        )
        return androidLog()
    }
}

internal fun boundedDiagnosticMessage(
    message: String,
    error: Throwable?,
    maxBytes: Int,
): String {
    val writer = Utf8TruncatingWriter(maxBytes.coerceAtLeast(0))
    writer.write(message)
    if (error != null) {
        writer.write("\n")
        PrintWriter(writer).use { printWriter -> error.printStackTrace(printWriter) }
    }
    val result = writer.result()
    return if (error == null) result else result.trimEnd()
}

internal fun formatDiagnosticTimestamp(timestampUnixMs: Long): String =
    DateTimeFormatter.ISO_INSTANT.format(Instant.ofEpochMilli(timestampUnixMs))
