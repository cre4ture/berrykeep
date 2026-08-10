package io.ironmesh.android.data

import java.io.Writer

internal class Utf8TruncatingWriter(
    private val maxBytes: Int,
) : Writer() {
    init {
        require(maxBytes >= 0) { "maxBytes must not be negative" }
    }

    private val content = StringBuilder(minOf(maxBytes, INITIAL_CAPACITY))
    private var contentBytes = 0
    private var pendingHighSurrogate: Char? = null
    private var truncated = false

    override fun write(characters: CharArray, offset: Int, length: Int) {
        requireRange(characters.size, offset, length)
        writeCharacters(length) { index -> characters[offset + index] }
    }

    override fun write(value: String, offset: Int, length: Int) {
        requireRange(value.length, offset, length)
        writeCharacters(length) { index -> value[offset + index] }
    }

    override fun flush() = Unit

    override fun close() {
        finishPendingSurrogate()
    }

    fun result(): String {
        finishPendingSurrogate()
        return content.toString()
    }

    private inline fun writeCharacters(length: Int, characterAt: (Int) -> Char) {
        if (truncated || length == 0) return

        var index = 0
        pendingHighSurrogate?.let { highSurrogate ->
            val first = characterAt(0)
            pendingHighSurrogate = null
            if (first.isLowSurrogate()) {
                appendCodePoint(Character.toCodePoint(highSurrogate, first))
                index = 1
            } else {
                appendCodePoint(REPLACEMENT_CODE_POINT)
            }
        }

        while (index < length && !truncated) {
            val character = characterAt(index)
            when {
                character.isHighSurrogate() && index + 1 < length && characterAt(index + 1).isLowSurrogate() -> {
                    appendCodePoint(Character.toCodePoint(character, characterAt(index + 1)))
                    index += 2
                }

                character.isHighSurrogate() -> {
                    pendingHighSurrogate = character
                    index += 1
                }

                character.isLowSurrogate() -> {
                    appendCodePoint(REPLACEMENT_CODE_POINT)
                    index += 1
                }

                else -> {
                    appendCodePoint(character.code)
                    index += 1
                }
            }
        }
    }

    private fun appendCodePoint(codePoint: Int) {
        val requiredBytes = utf8Length(codePoint)
        if (contentBytes + requiredBytes <= maxBytes) {
            content.appendCodePoint(codePoint)
            contentBytes += requiredBytes
        } else {
            markTruncated()
        }
    }

    private fun finishPendingSurrogate() {
        if (pendingHighSurrogate != null && !truncated) {
            pendingHighSurrogate = null
            appendCodePoint(REPLACEMENT_CODE_POINT)
        }
    }

    private fun markTruncated() {
        val marker = utf8Prefix(TRUNCATION_MARKER, maxBytes)
        val prefixBudget = maxBytes - utf8Length(marker)
        val prefix = utf8Prefix(content, prefixBudget)
        content.clear()
        content.append(prefix)
        content.append(marker)
        contentBytes = utf8Length(content)
        pendingHighSurrogate = null
        truncated = true
    }

    private fun requireRange(size: Int, offset: Int, length: Int) {
        require(offset >= 0 && length >= 0 && offset <= size - length) {
            "Invalid write range offset=$offset length=$length size=$size"
        }
    }

    companion object {
        private const val INITIAL_CAPACITY = 4 * 1024
        private const val REPLACEMENT_CODE_POINT = 0xFFFD
        private const val TRUNCATION_MARKER = "[truncated]"
    }
}

private fun utf8Prefix(value: CharSequence, maxBytes: Int): String {
    if (maxBytes <= 0 || value.isEmpty()) return ""

    var index = 0
    var bytes = 0
    while (index < value.length) {
        val character = value[index]
        val codePoint = when {
            character.isHighSurrogate() && index + 1 < value.length && value[index + 1].isLowSurrogate() ->
                Character.toCodePoint(character, value[index + 1])

            character.isSurrogate() -> 0xFFFD
            else -> character.code
        }
        val codePointBytes = utf8Length(codePoint)
        if (bytes + codePointBytes > maxBytes) break
        bytes += codePointBytes
        index += Character.charCount(codePoint)
    }
    return value.subSequence(0, index).toString()
}

internal fun utf8Length(value: CharSequence): Int {
    var bytes = 0L
    var index = 0
    while (index < value.length) {
        val character = value[index]
        val codePoint = when {
            character.isHighSurrogate() && index + 1 < value.length && value[index + 1].isLowSurrogate() ->
                Character.toCodePoint(character, value[index + 1])

            character.isSurrogate() -> 0xFFFD
            else -> character.code
        }
        bytes += utf8Length(codePoint)
        check(bytes <= Int.MAX_VALUE) { "UTF-8 length exceeds supported diagnostic size" }
        index += Character.charCount(codePoint)
    }
    return bytes.toInt()
}

private fun utf8Length(codePoint: Int): Int = when {
    codePoint <= 0x7F -> 1
    codePoint <= 0x7FF -> 2
    codePoint <= 0xFFFF -> 3
    else -> 4
}

private fun Char.isSurrogate(): Boolean = isHighSurrogate() || isLowSurrogate()
