package io.ironmesh.android.data

import java.io.PrintWriter
import java.io.StringWriter
import java.time.Instant
import java.time.format.DateTimeFormatter
import java.util.ArrayDeque

internal data class AndroidDiagnosticLogEntry(
    val timestampUnixMs: Long,
    val level: String,
    val tag: String,
    val message: String,
)

internal class AndroidDiagnosticLogBuffer(
    private val maxEntries: Int,
) {
    private val entries = ArrayDeque<AndroidDiagnosticLogEntry>()

    @Synchronized
    fun push(entry: AndroidDiagnosticLogEntry) {
        entries.addLast(entry)
        while (entries.size > maxEntries.coerceAtLeast(1)) {
            entries.removeFirst()
        }
    }

    @Synchronized
    fun renderText(): String = entries.joinToString(separator = "\n", postfix = if (entries.isEmpty()) "" else "\n") {
        "${formatDiagnosticTimestamp(it.timestampUnixMs)} ${it.level}/${it.tag}: ${it.message}"
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

    fun renderText(): String = buffer.renderText()

    private fun record(
        level: String,
        tag: String,
        message: String,
        error: Throwable?,
        androidLog: () -> Int,
    ): Int {
        val detail = if (error == null) {
            message
        } else {
            "$message\n${error.stackTraceText()}"
        }
        buffer.push(
            AndroidDiagnosticLogEntry(
                timestampUnixMs = System.currentTimeMillis(),
                level = level,
                tag = tag,
                message = detail,
            ),
        )
        return androidLog()
    }
}

internal fun buildAndroidDiagnosticLogExport(
    generatedAtUnixMs: Long,
    metadata: List<Pair<String, String>>,
    applicationLog: String,
    rustLog: String,
): String = buildString {
    appendLine("BerryKeep mobile diagnostic log")
    appendLine("Generated: ${formatDiagnosticTimestamp(generatedAtUnixMs)}")
    appendLine("This file can contain server addresses, device identifiers, paths, and error details.")
    metadata.forEach { (key, value) ->
        appendLine("$key: ${value.replace('\n', ' ')}")
    }
    appendLine()
    appendLine("=== Android application log ===")
    append(applicationLog.ifBlank { "(no Android application entries retained)\n" })
    if (!endsWith('\n')) {
        appendLine()
    }
    appendLine()
    appendLine("=== Rust tracing log ===")
    append(rustLog.ifBlank { "(no Rust tracing entries retained)\n" })
    if (!endsWith('\n')) {
        appendLine()
    }
}

private fun Throwable.stackTraceText(): String {
    val writer = StringWriter()
    PrintWriter(writer).use { printWriter -> printStackTrace(printWriter) }
    return writer.toString().trimEnd()
}

private fun formatDiagnosticTimestamp(timestampUnixMs: Long): String =
    DateTimeFormatter.ISO_INSTANT.format(Instant.ofEpochMilli(timestampUnixMs))
