package io.ironmesh.android.saf

import android.system.ErrnoException
import android.system.OsConstants
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class IronmeshSeekableFileCallbackInstrumentationTest {
    @Test
    fun randomReads_fetchOnlyAlignedChunksAndReuseTheBoundedCache() {
        val contents = ByteArray(5_000) { index -> (index % 251).toByte() }
        val requestedRanges = mutableListOf<Pair<Long, Int>>()
        val callback = IronmeshSeekableFileCallback(
            requestId = 1,
            remotePath = "large/video.mp4",
            objectSizeBytes = contents.size.toLong(),
            readRange = { offset, length ->
                requestedRanges += offset to length
                contents.copyOfRange(offset.toInt(), offset.toInt() + length)
            },
            onReleased = {},
            chunkSizeBytes = 1_024,
            maxCachedChunks = 2,
        )

        val first = ByteArray(80)
        assertEquals(first.size, callback.onRead(1_500, first.size, first))
        assertArrayEquals(contents.copyOfRange(1_500, 1_580), first)

        val cached = ByteArray(40)
        assertEquals(cached.size, callback.onRead(1_700, cached.size, cached))
        assertArrayEquals(contents.copyOfRange(1_700, 1_740), cached)
        assertEquals(listOf(1_024L to 1_024), requestedRanges)

        val tail = ByteArray(100)
        assertEquals(20, callback.onRead(4_980, tail.size, tail))
        assertArrayEquals(contents.copyOfRange(4_980, 5_000), tail.copyOf(20))
        assertEquals(listOf(1_024L to 1_024, 4_096L to 904), requestedRanges)
        callback.onRelease()
    }

    @Test
    fun cacheEviction_keepsMemoryBounded() {
        val requestedOffsets = mutableListOf<Long>()
        val callback = IronmeshSeekableFileCallback(
            requestId = 2,
            remotePath = "large/video.mp4",
            objectSizeBytes = 4_096,
            readRange = { offset, length ->
                requestedOffsets += offset
                ByteArray(length)
            },
            onReleased = {},
            chunkSizeBytes = 1_024,
            maxCachedChunks = 2,
        )

        listOf(0L, 1_024L, 2_048L, 0L).forEach { offset ->
            assertEquals(1, callback.onRead(offset, 1, ByteArray(1)))
        }
        assertEquals(listOf(0L, 1_024L, 2_048L, 0L), requestedOffsets)
        callback.onRelease()
    }

    @Test
    fun cancellation_stopsSubsequentRangeReads() {
        val callback = IronmeshSeekableFileCallback(
            requestId = 3,
            remotePath = "large/video.mp4",
            objectSizeBytes = 1_024,
            readRange = { _, length -> ByteArray(length) },
            onReleased = {},
        )
        callback.requestCancellation()

        val error = assertThrows(ErrnoException::class.java) {
            callback.onRead(0, 1, ByteArray(1))
        }
        assertEquals(OsConstants.ECANCELED, error.errno)
        callback.onRelease()
    }
}
