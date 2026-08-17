package io.ironmesh.android.saf

import android.os.ProxyFileDescriptorCallback
import android.os.SystemClock
import android.system.ErrnoException
import android.system.OsConstants
import io.ironmesh.android.data.AndroidDiagnosticLog as Log
import java.io.IOException
import java.util.LinkedHashMap
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Exposes a remote object through Android's seekable proxy-file API without staging the object.
 * Reads are aligned into bounded chunks so a multi-gigabyte video uses constant local memory.
 */
internal class IronmeshSeekableFileCallback(
    private val requestId: Long,
    private val remotePath: String,
    private val objectSizeBytes: Long,
    private val readRange: (offset: Long, length: Int) -> ByteArray,
    private val onReleased: () -> Unit,
    private val chunkSizeBytes: Int = DEFAULT_CHUNK_SIZE_BYTES,
    private val maxCachedChunks: Int = DEFAULT_MAX_CACHED_CHUNKS,
) : ProxyFileDescriptorCallback() {
    private val openedAtElapsedMs = SystemClock.elapsedRealtime()
    private val released = AtomicBoolean(false)
    private val cancellationRequested = AtomicBoolean(false)
    private val chunkCache = object : LinkedHashMap<Long, ByteArray>(maxCachedChunks, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<Long, ByteArray>?): Boolean =
            size > maxCachedChunks
    }
    private var readRequestCount = 0
    private var bytesServed = 0L
    private var rangeBytesFetched = 0L
    private var cacheHitCount = 0
    private var cacheMissCount = 0
    private var nonSequentialReadCount = 0
    private var nextSequentialOffset: Long? = null
    private var firstReadOffset: Long? = null
    private var lastReadEndOffset: Long? = null

    init {
        require(objectSizeBytes >= 0) { "object size must not be negative" }
        require(chunkSizeBytes > 0) { "chunk size must be positive" }
        require(maxCachedChunks > 0) { "chunk cache must contain at least one entry" }
    }

    override fun onGetSize(): Long {
        throwIfUnavailable("onGetSize")
        return objectSizeBytes
    }

    override fun onRead(
        offset: Long,
        size: Int,
        data: ByteArray,
    ): Int {
        if (offset < 0 || size < 0 || size > data.size) {
            throw ErrnoException("onRead", OsConstants.EINVAL)
        }
        throwIfUnavailable("onRead")
        if (size == 0 || offset >= objectSizeBytes) {
            return 0
        }

        val bytesRead = try {
            readInto(offset, minOf(size.toLong(), objectSizeBytes - offset).toInt(), data)
        } catch (error: ErrnoException) {
            throw error
        } catch (error: Exception) {
            Log.e(
                TAG,
                "SAF range read failed: request_id=$requestId path=$remotePath " +
                    "offset=$offset requested_bytes=$size",
                error,
            )
            throw ErrnoException("onRead", OsConstants.EIO, error)
        }

        recordRead(offset, size, bytesRead)
        return bytesRead
    }

    override fun onRelease() {
        if (!released.compareAndSet(false, true)) {
            return
        }

        try {
            synchronized(chunkCache) { chunkCache.clear() }
            Log.i(
                TAG,
                "SAF read released: request_id=$requestId path=$remotePath " +
                    "canceled=${cancellationRequested.get()} object_size_bytes=$objectSizeBytes " +
                    "read_requests=$readRequestCount bytes_served=$bytesServed " +
                    "range_bytes_fetched=$rangeBytesFetched cache_hits=$cacheHitCount " +
                    "cache_misses=$cacheMissCount non_sequential_reads=$nonSequentialReadCount " +
                    "first_read_offset=${firstReadOffset ?: "<none>"} " +
                    "last_read_end_offset=${lastReadEndOffset ?: "<none>"} " +
                    "elapsed_ms=${SystemClock.elapsedRealtime() - openedAtElapsedMs}",
            )
        } finally {
            onReleased()
        }
    }

    fun requestCancellation() {
        if (cancellationRequested.compareAndSet(false, true)) {
            Log.i(
                TAG,
                "SAF read cancellation requested: request_id=$requestId path=$remotePath",
            )
        }
    }

    private fun readInto(offset: Long, byteCount: Int, destination: ByteArray): Int {
        var copied = 0
        while (copied < byteCount) {
            throwIfUnavailable("onRead")
            val absoluteOffset = offset + copied
            val chunkStart = (absoluteOffset / chunkSizeBytes) * chunkSizeBytes
            val chunk = loadChunk(chunkStart)
            val offsetInChunk = (absoluteOffset - chunkStart).toInt()
            val copyLength = minOf(byteCount - copied, chunk.size - offsetInChunk)
            if (copyLength <= 0) {
                throw IOException("Range source returned no data before the advertised object end")
            }
            chunk.copyInto(
                destination,
                destinationOffset = copied,
                startIndex = offsetInChunk,
                endIndex = offsetInChunk + copyLength,
            )
            copied += copyLength
        }
        return copied
    }

    private fun loadChunk(chunkStart: Long): ByteArray {
        synchronized(chunkCache) {
            chunkCache[chunkStart]?.let { cached ->
                cacheHitCount += 1
                return cached
            }
        }

        cacheMissCount += 1
        val length = minOf(chunkSizeBytes.toLong(), objectSizeBytes - chunkStart).toInt()
        if (length <= 0) {
            return ByteArray(0)
        }
        throwIfUnavailable("onRead")
        val downloaded = readRange(chunkStart, length)
        throwIfUnavailable("onRead")
        if (downloaded.size != length) {
            throw IOException(
                "Range size mismatch at offset $chunkStart: expected=$length actual=${downloaded.size}",
            )
        }
        rangeBytesFetched += downloaded.size
        synchronized(chunkCache) { chunkCache[chunkStart] = downloaded }
        return downloaded
    }

    private fun throwIfUnavailable(operation: String) {
        if (released.get()) {
            throw ErrnoException(operation, OsConstants.EBADF)
        }
        if (cancellationRequested.get()) {
            throw ErrnoException(operation, OsConstants.ECANCELED)
        }
    }

    private fun recordRead(
        offset: Long,
        requestedSize: Int,
        bytesRead: Int,
    ) {
        val isNonSequential = nextSequentialOffset?.let { it != offset } ?: false
        if (isNonSequential) {
            nonSequentialReadCount += 1
        }
        readRequestCount += 1
        bytesServed += bytesRead
        if (firstReadOffset == null) {
            firstReadOffset = offset
        }
        val readEndOffset = offset + bytesRead
        lastReadEndOffset = readEndOffset
        nextSequentialOffset = readEndOffset

        if (readRequestCount <= MAX_DETAILED_READ_LOGS || isNonSequential) {
            Log.d(
                TAG,
                "SAF read served: request_id=$requestId path=$remotePath offset=$offset " +
                    "requested_bytes=$requestedSize returned_bytes=$bytesRead " +
                    "non_sequential=$isNonSequential",
            )
        }
    }

    private companion object {
        private const val TAG = "IronmeshDocumentsProvider"
        private const val MAX_DETAILED_READ_LOGS = 8
        // Four MiB amortizes the SDK's metadata request across sequential playback while two
        // cached chunks keep each open descriptor's steady-state cache bounded to eight MiB.
        private const val DEFAULT_CHUNK_SIZE_BYTES = 4 * 1024 * 1024
        private const val DEFAULT_MAX_CACHED_CHUNKS = 2
    }
}
