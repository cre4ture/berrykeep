package io.ironmesh.android

import android.content.ContentResolver
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.ImageDecoder
import android.net.Uri
import android.os.Build
import io.ironmesh.android.data.AndroidDiagnosticLog as Log
import java.io.File

object DocumentBitmapLoader {
    fun load(
        context: Context,
        contentResolver: ContentResolver,
        documentUri: Uri,
        maxDimensionPx: Int,
    ): Bitmap? {
        return runCatching {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                decodeWithImageDecoder(contentResolver, documentUri, maxDimensionPx)
            } else {
                decodeWithBitmapFactory(
                    context = context,
                    contentResolver = contentResolver,
                    documentUri = documentUri,
                    maxDimensionPx = maxDimensionPx,
                )
            }
        }.onFailure { error ->
            Log.w("MainActivity", "Full image load failed for $documentUri", error)
        }.getOrNull()
    }

    fun decodeWithImageDecoder(
        contentResolver: ContentResolver,
        documentUri: Uri,
        maxDimensionPx: Int,
    ): Bitmap {
        val source = ImageDecoder.createSource(contentResolver, documentUri)
        return decodeWithImageDecoder(source, maxDimensionPx)
    }

    fun cacheDocument(
        context: Context,
        contentResolver: ContentResolver,
        documentUri: Uri,
    ): File {
        val stagedFile = File.createTempFile("ironmesh-viewer-", ".img", context.cacheDir)
        try {
            val inputStream = contentResolver.openInputStream(documentUri)
                ?: error("Failed to open image stream")
            inputStream.use { input ->
                stagedFile.outputStream().use { output ->
                    input.copyTo(output)
                    output.flush()
                }
            }
            return stagedFile
        } catch (error: Exception) {
            stagedFile.delete()
            throw error
        }
    }

    fun load(
        sourceFile: File,
        maxDimensionPx: Int?,
    ): Bitmap? {
        return runCatching {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                decodeWithImageDecoder(ImageDecoder.createSource(sourceFile), maxDimensionPx)
            } else {
                decodeWithBitmapFactory(sourceFile, maxDimensionPx)
            }
        }.onFailure { error ->
            Log.w("MainActivity", "Cached full image load failed for ${sourceFile.name}", error)
        }.getOrNull()
    }

    private fun decodeWithImageDecoder(
        source: ImageDecoder.Source,
        maxDimensionPx: Int?,
    ): Bitmap {
        return ImageDecoder.decodeBitmap(source) { decoder, info, _ ->
            val targetSize = maxDimensionPx?.let { maxDimension ->
                computeTargetSize(
                    width = info.size.width,
                    height = info.size.height,
                    maxDimensionPx = maxDimension,
                )
            }
            decoder.setAllocator(ImageDecoder.ALLOCATOR_SOFTWARE)
            if (targetSize != null) {
                decoder.setTargetSize(
                    targetSize.width,
                    targetSize.height,
                )
            }
        }
    }

    fun decodeWithBitmapFactory(
        context: Context,
        contentResolver: ContentResolver,
        documentUri: Uri,
        maxDimensionPx: Int,
    ): Bitmap {
        val stagedFile = cacheDocument(context, contentResolver, documentUri)
        try {
            return decodeWithBitmapFactory(stagedFile, maxDimensionPx)
        } finally {
            stagedFile.delete()
        }
    }

    private fun decodeWithBitmapFactory(
        sourceFile: File,
        maxDimensionPx: Int?,
    ): Bitmap {
        val bounds = BitmapFactory.Options().apply {
            inJustDecodeBounds = true
        }
        BitmapFactory.decodeFile(sourceFile.absolutePath, bounds)

        if (bounds.outWidth <= 0 || bounds.outHeight <= 0) {
            error("Failed to decode image bounds")
        }

        val sampleSize = maxDimensionPx?.let { maxDimension ->
            computeInSampleSize(
                width = bounds.outWidth,
                height = bounds.outHeight,
                maxDimensionPx = maxDimension,
            )
        } ?: 1
        val decodeOptions = BitmapFactory.Options().apply {
            inSampleSize = sampleSize
        }
        return BitmapFactory.decodeFile(sourceFile.absolutePath, decodeOptions)
            ?: error("Bitmap decode returned null")
    }

    private fun computeTargetSize(
        width: Int,
        height: Int,
        maxDimensionPx: Int,
    ): ImageDecodeSize? {
        val sourceMaxDimension = maxOf(width, height)
        if (width <= 0 || height <= 0 || maxDimensionPx <= 0 || sourceMaxDimension <= maxDimensionPx) {
            return null
        }

        val scale = maxDimensionPx.toFloat() / sourceMaxDimension
        return ImageDecodeSize(
            width = (width * scale).toInt().coerceAtLeast(1),
            height = (height * scale).toInt().coerceAtLeast(1),
        )
    }

    private fun computeInSampleSize(
        width: Int,
        height: Int,
        maxDimensionPx: Int,
    ): Int {
        if (width <= 0 || height <= 0 || maxDimensionPx <= 0) {
            return 1
        }

        var sampleSize = 1
        while (width / sampleSize > maxDimensionPx || height / sampleSize > maxDimensionPx) {
            sampleSize *= 2
        }
        return sampleSize.coerceAtLeast(1)
    }

    private data class ImageDecodeSize(
        val width: Int,
        val height: Int,
    )
}
