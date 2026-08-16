package io.ironmesh.android.saf

import android.os.ProxyFileDescriptorCallback
import android.os.SystemClock
import android.system.ErrnoException
import android.system.OsConstants
import io.ironmesh.android.data.AndroidDiagnosticLog as Log
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.io.OutputStream
import java.io.RandomAccessFile
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Exposes a lazily downloaded remote object through Android's seekable proxy-file API.
 * Each descriptor owns one staged file so independent callers also have independent offsets.
 */
internal class IronmeshSeekableFileCallback(
    private val requestId: Long,
    private val remotePath: String,
    private val expectedSizeBytes: Long?,
    private val cacheDir: File,
    private val downloadTo: (OutputStream) -> Unit,
    private val onReleased: () -> Unit,
) : ProxyFileDescriptorCallback() {
    private val openedAtElapsedMs = SystemClock.elapsedRealtime()
    private val released = AtomicBoolean(false)
    private val cancellationRequested = AtomicBoolean(false)
    private var stagedFile: File? = null
    private var stagedReader: RandomAccessFile? = null
    private var stagingFailure: Throwable? = null
    private var downloadedSizeBytes: Long? = null
    private var readRequestCount = 0
    private var bytesServed = 0L
    private var nonSequentialReadCount = 0
    private var nextSequentialOffset: Long? = null
    private var firstReadOffset: Long? = null
    private var lastReadEndOffset: Long? = null

    override fun onGetSize(): Long {
        val size = expectedSizeBytes ?: stagedSize("onGetSize")
        Log.d(
            TAG,
            "SAF read size: request_id=$requestId path=$remotePath size_bytes=$size " +
                "source=${if (expectedSizeBytes == null) "download" else "store_index"}",
        )
        return size
    }

    override fun onRead(
        offset: Long,
        size: Int,
        data: ByteArray,
    ): Int {
        if (offset < 0 || size < 0 || size > data.size) {
            throw ErrnoException("onRead", OsConstants.EINVAL)
        }
        if (released.get()) {
            throw ErrnoException("onRead", OsConstants.EBADF)
        }
        if (size == 0) {
            return 0
        }

        val reader = ensureStaged("onRead")
        val fileSize = downloadedSizeBytes
            ?: failStaging("onRead", IOException("staged download size is unavailable"))
        val bytesRead = if (offset >= fileSize) {
            0
        } else {
            val readableBytes = minOf(size.toLong(), fileSize - offset).toInt()
            try {
                reader.seek(offset)
                reader.read(data, 0, readableBytes).coerceAtLeast(0)
            } catch (error: IOException) {
                logReadFailure(offset, size, error)
                throw ErrnoException("onRead", OsConstants.EIO, error)
            }
        }

        recordRead(offset, size, bytesRead)
        return bytesRead
    }

    override fun onRelease() {
        if (!released.compareAndSet(false, true)) {
            return
        }

        try {
            runCatching { stagedReader?.close() }
                .onFailure { error ->
                    Log.w(
                        TAG,
                        "Failed to close SAF staged file: request_id=$requestId path=$remotePath",
                        error,
                    )
                }
            stagedReader = null
            stagedFile?.let { file ->
                if (file.exists() && !file.delete()) {
                    Log.w(
                        TAG,
                        "Failed to delete SAF staged file: request_id=$requestId path=$remotePath",
                    )
                }
            }
            stagedFile = null

            Log.i(
                TAG,
                "SAF read released: request_id=$requestId path=$remotePath " +
                    "canceled=${cancellationRequested.get()} " +
                    "downloaded_size_bytes=${downloadedSizeBytes ?: "<none>"} " +
                    "read_requests=$readRequestCount bytes_served=$bytesServed " +
                    "non_sequential_reads=$nonSequentialReadCount " +
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

    private fun ensureStaged(operation: String): RandomAccessFile {
        stagedReader?.let { return it }
        stagingFailure?.let { error ->
            throw ErrnoException(operation, OsConstants.EIO, error)
        }
        if (released.get()) {
            throw ErrnoException(operation, OsConstants.EBADF)
        }

        val downloadStartedAt = SystemClock.elapsedRealtime()
        Log.i(
            TAG,
            "SAF download started: request_id=$requestId path=$remotePath " +
                "expected_size_bytes=${expectedSizeBytes ?: "<unknown>"}",
        )
        val file = try {
            File.createTempFile("ironmesh-saf-$requestId-", ".bin", cacheDir)
        } catch (error: IOException) {
            failStaging(operation, error)
        }

        try {
            FileOutputStream(file).buffered().use { output ->
                downloadTo(output)
                output.flush()
            }
            val actualSizeBytes = file.length()
            if (expectedSizeBytes != null && actualSizeBytes != expectedSizeBytes) {
                throw IOException(
                    "downloaded size mismatch: expected=$expectedSizeBytes actual=$actualSizeBytes",
                )
            }

            val reader = RandomAccessFile(file, "r")
            stagedFile = file
            stagedReader = reader
            downloadedSizeBytes = actualSizeBytes
            Log.i(
                TAG,
                "SAF download completed: request_id=$requestId path=$remotePath " +
                    "size_bytes=$actualSizeBytes " +
                    "elapsed_ms=${SystemClock.elapsedRealtime() - downloadStartedAt}",
            )
            return reader
        } catch (error: Exception) {
            file.delete()
            failStaging(operation, error)
        }
    }

    private fun stagedSize(operation: String): Long {
        ensureStaged(operation)
        return downloadedSizeBytes
            ?: failStaging(operation, IOException("staged download size is unavailable"))
    }

    private fun failStaging(
        operation: String,
        error: Throwable,
    ): Nothing {
        stagingFailure = error
        Log.e(
            TAG,
            "SAF download failed: request_id=$requestId path=$remotePath operation=$operation",
            error,
        )
        throw ErrnoException(operation, OsConstants.EIO, error)
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

    private fun logReadFailure(
        offset: Long,
        requestedSize: Int,
        error: IOException,
    ) {
        Log.e(
            TAG,
            "SAF read failed: request_id=$requestId path=$remotePath offset=$offset " +
                "requested_bytes=$requestedSize",
            error,
        )
    }

    private companion object {
        private const val TAG = "IronmeshDocumentsProvider"
        private const val MAX_DETAILED_READ_LOGS = 8
    }
}
