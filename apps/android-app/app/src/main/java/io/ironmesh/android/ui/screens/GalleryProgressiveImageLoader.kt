package io.ironmesh.android.ui.screens

import android.content.ContentResolver
import android.content.Context
import android.net.Uri
import io.ironmesh.android.DocumentBitmapLoader
import java.io.File
import java.util.LinkedHashMap

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
    private val sourceFiles = LinkedHashMap<String, File>(
        MAX_CACHED_GALLERY_ORIGINALS + 1,
        0.75f,
        true,
    )

    @Synchronized
    fun fileFor(
        context: Context,
        contentResolver: ContentResolver,
        documentUri: Uri,
    ): File {
        val cacheKey = documentUri.toString()
        sourceFiles[cacheKey]?.takeIf(File::isFile)?.let { return it }
        sourceFiles.remove(cacheKey)?.delete()

        val stagedFile = DocumentBitmapLoader.cacheDocument(context, contentResolver, documentUri)
        sourceFiles[cacheKey] = stagedFile
        while (sourceFiles.size > MAX_CACHED_GALLERY_ORIGINALS) {
            val oldest = sourceFiles.entries.iterator().next()
            oldest.value.delete()
            sourceFiles.remove(oldest.key)
        }
        return stagedFile
    }

    @Synchronized
    fun clear() {
        sourceFiles.values.forEach(File::delete)
        sourceFiles.clear()
    }
}
