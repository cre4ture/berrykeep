package io.berrykeep.android.ui.screens

import android.content.ContentResolver
import android.content.Context
import android.net.Uri
import io.berrykeep.android.DocumentBitmapLoader
import java.io.File
import java.util.LinkedHashMap
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ExecutionException

private const val GALLERY_FULL_RESOLUTION_ZOOM_THRESHOLD = 1.01f
private const val GALLERY_HIGH_RESOLUTION_ZOOM_THRESHOLD = 2f
private const val GALLERY_ORIGINAL_RESOLUTION_ZOOM_THRESHOLD = 3.5f
private const val GALLERY_DETAIL_MAX_DECODE_DIMENSION_PX = 3072
private const val GALLERY_HIGH_MAX_DECODE_DIMENSION_PX = 6144
private const val MAX_CACHED_GALLERY_ORIGINALS = 3

internal enum class GalleryImageResolution(
    val maxDecodeDimensionPx: Int?,
) {
    DETAIL(GALLERY_DETAIL_MAX_DECODE_DIMENSION_PX),
    HIGH(GALLERY_HIGH_MAX_DECODE_DIMENSION_PX),
    ORIGINAL(null),
}

internal fun galleryImageResolutionForScale(scale: Float): GalleryImageResolution? =
    when {
        scale > GALLERY_ORIGINAL_RESOLUTION_ZOOM_THRESHOLD -> GalleryImageResolution.ORIGINAL
        scale > GALLERY_HIGH_RESOLUTION_ZOOM_THRESHOLD -> GalleryImageResolution.HIGH
        scale > GALLERY_FULL_RESOLUTION_ZOOM_THRESHOLD -> GalleryImageResolution.DETAIL
        else -> null
    }

internal fun shouldLoadGalleryImageResolution(
    requestedResolution: GalleryImageResolution?,
    loadedResolution: GalleryImageResolution?,
): Boolean =
    requestedResolution != null &&
        (loadedResolution == null || loadedResolution.ordinal < requestedResolution.ordinal)

/**
 * Holds a small set of source files for the life of one fullscreen gallery session.
 * Decoding additional zoom levels from the local copy avoids repeated SAF downloads.
 */
internal class GalleryOriginalImageCache {
    private val lock = Any()
    private val sourceFiles = LinkedHashMap<String, File>(
        MAX_CACHED_GALLERY_ORIGINALS + 1,
        0.75f,
        true,
    )
    private val inFlightDownloads = mutableMapOf<String, InFlightDownload>()
    private var generation = 0L

    fun fileFor(
        context: Context,
        contentResolver: ContentResolver,
        documentUri: Uri,
    ): File = fileFor(documentUri.toString()) {
        DocumentBitmapLoader.cacheDocument(context, contentResolver, documentUri)
    }

    /**
     * Stages each image once while allowing unrelated images to download in parallel.
     *
     * The staging lambda deliberately runs outside [lock]: content resolver reads can take
     * arbitrarily long and must not serialize the whole fullscreen gallery.
     */
    internal fun fileFor(
        cacheKey: String,
        stageDocument: () -> File,
    ): File {
        val download = synchronized(lock) {
            sourceFiles[cacheKey]?.takeIf(File::isFile)?.let { return it }
            sourceFiles.remove(cacheKey)?.delete()

            inFlightDownloads[cacheKey] ?: InFlightDownload(generation).also { created ->
                inFlightDownloads[cacheKey] = created
            }
        }

        if (!download.claimStaging()) {
            return download.awaitFile()
        }

        return try {
            val stagedFile = stageDocument()
            synchronized(lock) {
                if (generation == download.generation && inFlightDownloads[cacheKey] === download) {
                    inFlightDownloads.remove(cacheKey)
                    sourceFiles[cacheKey] = stagedFile
                    evictExcessFiles()
                    download.complete(stagedFile)
                } else {
                    stagedFile.delete()
                    download.fail(CacheInvalidatedException())
                }
            }
            download.awaitFile()
        } catch (error: Throwable) {
            synchronized(lock) {
                if (inFlightDownloads[cacheKey] === download) {
                    inFlightDownloads.remove(cacheKey)
                }
                download.fail(error)
            }
            throw error
        }
    }

    fun clear() {
        synchronized(lock) {
            generation += 1
            sourceFiles.values.forEach(File::delete)
            sourceFiles.clear()
            inFlightDownloads.values.forEach { it.fail(CacheInvalidatedException()) }
            inFlightDownloads.clear()
        }
    }

    private fun evictExcessFiles() {
        while (sourceFiles.size > MAX_CACHED_GALLERY_ORIGINALS) {
            val oldest = sourceFiles.entries.iterator().next()
            oldest.value.delete()
            sourceFiles.remove(oldest.key)
        }
    }

    private class InFlightDownload(
        val generation: Long,
    ) {
        private val file = CompletableFuture<File>()
        private var stagingClaimed = false

        fun claimStaging(): Boolean = synchronized(this) {
            if (stagingClaimed) {
                false
            } else {
                stagingClaimed = true
                true
            }
        }

        fun complete(stagedFile: File) {
            file.complete(stagedFile)
        }

        fun fail(error: Throwable) {
            file.completeExceptionally(error)
        }

        fun awaitFile(): File = try {
            file.get()
        } catch (error: ExecutionException) {
            throw error.cause ?: error
        }
    }

    private class CacheInvalidatedException : IllegalStateException(
        "Gallery image cache was cleared while the image was loading",
    )
}
