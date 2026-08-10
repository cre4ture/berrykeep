package io.ironmesh.android.data

import java.io.Writer

internal fun writeAndroidDiagnosticLogExport(
    writer: Writer,
    generatedAtUnixMs: Long,
    metadata: List<Pair<String, String>>,
    applicationLogWriter: (Writer) -> Unit,
    rustLog: String,
) {
    writer.write("BerryKeep mobile diagnostic log\n")
    writer.write("Generated: ${formatDiagnosticTimestamp(generatedAtUnixMs)}\n")
    writer.write("This file can contain server addresses, device identifiers, paths, and error details.\n")
    metadata.forEach { (key, value) ->
        writer.write(key)
        writer.write(": ")
        writeSingleLine(writer, value)
        writer.write("\n")
    }
    writer.write("\n=== Android application log ===\n")
    writeLogSection(writer, "(no Android application entries retained)", applicationLogWriter)
    writer.write("\n=== Rust tracing log ===\n")
    writeLogSection(writer, "(no Rust tracing entries retained)") { sectionWriter ->
        if (rustLog.isNotBlank()) {
            sectionWriter.write(rustLog)
        }
    }
}

private fun writeLogSection(
    writer: Writer,
    emptyMessage: String,
    content: (Writer) -> Unit,
) {
    val trackingWriter = TrailingCharacterTrackingWriter(writer)
    content(trackingWriter)
    if (!trackingWriter.hasContent) {
        writer.write(emptyMessage)
        writer.write("\n")
    } else if (trackingWriter.lastCharacter != '\n') {
        writer.write("\n")
    }
}

private fun writeSingleLine(writer: Writer, value: String) {
    var segmentStart = 0
    value.forEachIndexed { index, character ->
        if (character == '\n' || character == '\r') {
            if (segmentStart < index) {
                writer.write(value, segmentStart, index - segmentStart)
            }
            writer.write(" ")
            segmentStart = index + 1
        }
    }
    if (segmentStart < value.length) {
        writer.write(value, segmentStart, value.length - segmentStart)
    }
}

private class TrailingCharacterTrackingWriter(
    private val delegate: Writer,
) : Writer() {
    var hasContent: Boolean = false
        private set
    var lastCharacter: Char? = null
        private set

    override fun write(characters: CharArray, offset: Int, length: Int) {
        delegate.write(characters, offset, length)
        track(length) { characters[offset + length - 1] }
    }

    override fun write(value: String, offset: Int, length: Int) {
        delegate.write(value, offset, length)
        track(length) { value[offset + length - 1] }
    }

    override fun flush() = delegate.flush()

    override fun close() = Unit

    private inline fun track(length: Int, trailingCharacter: () -> Char) {
        if (length > 0) {
            hasContent = true
            lastCharacter = trailingCharacter()
        }
    }
}
