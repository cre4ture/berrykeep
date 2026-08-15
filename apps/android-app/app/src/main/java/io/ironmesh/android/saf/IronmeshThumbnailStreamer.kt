package io.ironmesh.android.saf

import io.ironmesh.android.api.StoreIndexEntry
import io.ironmesh.android.data.IronmeshRepository
import java.io.FileNotFoundException
import java.io.OutputStream
import java.net.URLEncoder
import java.nio.charset.StandardCharsets

internal const val MOBILE_VIEWER_THUMBNAIL_MAX_DIMENSION_PX = 1536

internal enum class IronmeshThumbnailProfile(
    val queryValue: String?,
) {
    GRID(null),
    MOBILE_VIEWER("mobile_viewer"),
}

internal fun thumbnailProfileForRequestedSize(
    widthPx: Int,
    heightPx: Int,
): IronmeshThumbnailProfile =
    if (maxOf(widthPx, heightPx) >= MOBILE_VIEWER_THUMBNAIL_MAX_DIMENSION_PX) {
        IronmeshThumbnailProfile.MOBILE_VIEWER
    } else {
        IronmeshThumbnailProfile.GRID
    }

internal interface IronmeshThumbnailStreamDataSource {
    suspend fun streamRelativeUrlTo(
        connectionInput: String,
        relativeUrl: String,
        output: OutputStream,
        serverCaPem: String? = null,
        clientIdentityJson: String? = null,
    )
}

internal class IronmeshRepositoryThumbnailDataSource(
    private val repository: IronmeshRepository,
) : IronmeshThumbnailStreamDataSource {
    override suspend fun streamRelativeUrlTo(
        connectionInput: String,
        relativeUrl: String,
        output: OutputStream,
        serverCaPem: String?,
        clientIdentityJson: String?,
    ) {
        repository.streamRelativeUrlTo(
            connectionInput,
            relativeUrl,
            output,
            serverCaPem,
            clientIdentityJson,
        )
    }
}

internal class IronmeshThumbnailStreamer(
    private val dataSource: IronmeshThumbnailStreamDataSource,
) {
    suspend fun streamTo(
        connectionInput: String,
        entry: StoreIndexEntry,
        output: OutputStream,
        serverCaPem: String? = null,
        clientIdentityJson: String? = null,
        profile: IronmeshThumbnailProfile = IronmeshThumbnailProfile.GRID,
    ) {
        val baseThumbnailUrl = entry.media?.thumbnail?.url?.trim()?.takeIf { it.isNotEmpty() }
            ?: buildGeneratedThumbnailUrl(entry)
        val thumbnailUrl = applyProfile(baseThumbnailUrl, profile)
        dataSource.streamRelativeUrlTo(
            connectionInput,
            thumbnailUrl,
            output,
            serverCaPem,
            clientIdentityJson,
        )
    }

    private fun buildGeneratedThumbnailUrl(entry: StoreIndexEntry): String {
        if (!supportsGeneratedThumbnail(entry)) {
            throw FileNotFoundException("thumbnail not available")
        }

        val encodedKey = URLEncoder.encode(entry.path, StandardCharsets.UTF_8.toString())
            .replace("+", "%20")
        return "/media/thumbnail?key=$encodedKey"
    }

    private fun applyProfile(
        relativeUrl: String,
        profile: IronmeshThumbnailProfile,
    ): String {
        val profileValue = profile.queryValue ?: return relativeUrl
        val fragmentStart = relativeUrl.indexOf('#')
        val withoutFragment = if (fragmentStart >= 0) relativeUrl.substring(0, fragmentStart) else relativeUrl
        val fragment = if (fragmentStart >= 0) relativeUrl.substring(fragmentStart) else ""
        val queryStart = withoutFragment.indexOf('?')
        val path = if (queryStart >= 0) withoutFragment.substring(0, queryStart) else withoutFragment
        val existingParameters = if (queryStart >= 0) {
            withoutFragment.substring(queryStart + 1)
                .split('&')
                .filter { parameter ->
                    parameter.isNotBlank() && parameter.substringBefore('=') != "profile"
                }
        } else {
            emptyList()
        }
        val parameters = existingParameters + "profile=$profileValue"
        return "$path?${parameters.joinToString("&")}$fragment"
    }

    private fun supportsGeneratedThumbnail(entry: StoreIndexEntry): Boolean {
        val mimeType = entry.media?.mime_type?.lowercase()
        if (mimeType?.startsWith("image/") == true || mimeType?.startsWith("video/") == true) {
            return true
        }

        val mediaType = entry.media?.media_type?.lowercase()
        if (mediaType == "image" || mediaType == "video") {
            return true
        }

        val extension = entry.path.substringAfterLast('.', "").lowercase()
        return extension in THUMBNAIL_IMAGE_EXTENSIONS || extension in THUMBNAIL_VIDEO_EXTENSIONS
    }

    private companion object {
        val THUMBNAIL_IMAGE_EXTENSIONS = setOf(
            "avif",
            "bmp",
            "gif",
            "heic",
            "heif",
            "jpeg",
            "jpg",
            "png",
            "webp",
        )
        val THUMBNAIL_VIDEO_EXTENSIONS = setOf(
            "m4v",
            "mkv",
            "mov",
            "mp4",
            "ogv",
            "webm",
        )
    }
}
