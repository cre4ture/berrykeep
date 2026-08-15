package io.ironmesh.android.saf

import io.ironmesh.android.api.StoreIndexEntry
import io.ironmesh.android.api.StoreIndexMedia
import io.ironmesh.android.api.StoreIndexThumbnail
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test
import java.io.ByteArrayOutputStream
import java.io.FileNotFoundException
import java.io.OutputStream

class IronmeshThumbnailStreamerTest {
    @Test
    fun thumbnailProfileForRequestedSize_keepsGridRequestsSmall() {
        assertEquals(
            IronmeshThumbnailProfile.GRID,
            thumbnailProfileForRequestedSize(widthPx = 384, heightPx = 384),
        )
        assertEquals(
            IronmeshThumbnailProfile.MOBILE_VIEWER,
            thumbnailProfileForRequestedSize(
                widthPx = MOBILE_VIEWER_THUMBNAIL_MAX_DIMENSION_PX,
                heightPx = 800,
            ),
        )
    }

    @Test
    fun streamTo_usesRemoteThumbnailWhenAvailable() = runBlocking {
        val dataSource = FakeThumbnailDataSource(
            thumbnailBytes = "thumb".toByteArray(),
        )
        val streamer = IronmeshThumbnailStreamer(dataSource)
        val output = ByteArrayOutputStream()

        streamer.streamTo(
            connectionInput = "{\"bootstrap\":true}",
            entry = sampleImageEntry(),
            output = output,
        )

        assertEquals(1, dataSource.relativeUrlCallCount)
        assertEquals("/media/thumbnail?key=gallery%2Fcat.png", dataSource.lastRelativeUrl)
        assertArrayEquals("thumb".toByteArray(), output.toByteArray())
    }

    @Test
    fun streamTo_fallsBackToGeneratedThumbnailWhenMetadataIsMissing() = runBlocking {
        val dataSource = FakeThumbnailDataSource(
            thumbnailBytes = "thumb".toByteArray(),
        )
        val streamer = IronmeshThumbnailStreamer(dataSource)
        val output = ByteArrayOutputStream()

        streamer.streamTo(
            connectionInput = "{\"bootstrap\":true}",
            entry = sampleImageEntry(
                path = "gallery/cat one.png",
                thumbnail = null,
            ),
            output = output,
        )

        assertEquals(1, dataSource.relativeUrlCallCount)
        assertEquals(
            "/media/thumbnail?key=gallery%2Fcat%20one.png",
            dataSource.lastRelativeUrl,
        )
        assertArrayEquals("thumb".toByteArray(), output.toByteArray())
    }

    @Test
    fun streamTo_requestsMobileViewerProfileForFullscreenPreview() = runBlocking {
        val dataSource = FakeThumbnailDataSource(
            thumbnailBytes = "preview".toByteArray(),
        )
        val streamer = IronmeshThumbnailStreamer(dataSource)
        val output = ByteArrayOutputStream()

        streamer.streamTo(
            connectionInput = "{\"bootstrap\":true}",
            entry = sampleImageEntry(thumbnail = null),
            output = output,
            profile = IronmeshThumbnailProfile.MOBILE_VIEWER,
        )

        assertEquals(
            "/media/thumbnail?key=gallery%2Fcat.png&profile=mobile_viewer",
            dataSource.lastRelativeUrl,
        )
        assertArrayEquals("preview".toByteArray(), output.toByteArray())
    }

    @Test
    fun streamTo_replacesAdvertisedProfileWithoutDroppingVersion() = runBlocking {
        val dataSource = FakeThumbnailDataSource(
            thumbnailBytes = "preview".toByteArray(),
        )
        val streamer = IronmeshThumbnailStreamer(dataSource)

        streamer.streamTo(
            connectionInput = "{\"bootstrap\":true}",
            entry = sampleImageEntry(
                thumbnail = sampleThumbnail().copy(
                    url = "/media/thumbnail?key=gallery%2Fcat.png&profile=grid&version=v1",
                ),
            ),
            output = ByteArrayOutputStream(),
            profile = IronmeshThumbnailProfile.MOBILE_VIEWER,
        )

        assertEquals(
            "/media/thumbnail?key=gallery%2Fcat.png&version=v1&profile=mobile_viewer",
            dataSource.lastRelativeUrl,
        )
    }

    @Test
    fun streamTo_throwsWhenEntryDoesNotLookLikeMedia() = runBlocking {
        val dataSource = FakeThumbnailDataSource(
            thumbnailBytes = "thumb".toByteArray(),
        )
        val streamer = IronmeshThumbnailStreamer(dataSource)
        val output = ByteArrayOutputStream()

        assertThrows(FileNotFoundException::class.java) {
            runBlocking {
                streamer.streamTo(
                    connectionInput = "{\"bootstrap\":true}",
                    entry = StoreIndexEntry(
                        path = "docs/readme.txt",
                        entry_type = "key",
                        media = null,
                    ),
                    output = output,
                )
            }
        }
        assertEquals(0, dataSource.relativeUrlCallCount)
    }

    @Test
    fun streamTo_propagatesThumbnailFetchFailure() = runBlocking {
        val dataSource = FakeThumbnailDataSource(
            thumbnailBytes = "thumb".toByteArray(),
            relativeUrlFailure = IllegalStateException("direct endpoint unavailable"),
        )
        val streamer = IronmeshThumbnailStreamer(dataSource)
        val output = ByteArrayOutputStream()

        assertThrows(IllegalStateException::class.java) {
            runBlocking {
                streamer.streamTo(
                    connectionInput = "{\"bootstrap\":true}",
                    entry = sampleImageEntry(),
                    output = output,
                )
            }
        }
        assertEquals(1, dataSource.relativeUrlCallCount)
    }

    private fun sampleImageEntry(
        path: String = "gallery/cat.png",
        thumbnail: StoreIndexThumbnail? = sampleThumbnail(),
    ): StoreIndexEntry {
        return StoreIndexEntry(
            path = path,
            entry_type = "key",
            media = StoreIndexMedia(
                status = "ready",
                content_fingerprint = "cfp-cat",
                media_type = "image",
                mime_type = "image/png",
                width = 4,
                height = 3,
                thumbnail = thumbnail,
            ),
        )
    }

    private class FakeThumbnailDataSource(
        private val thumbnailBytes: ByteArray,
        private val relativeUrlFailure: Exception? = null,
    ) : IronmeshThumbnailStreamDataSource {
        var relativeUrlCallCount: Int = 0
        var lastRelativeUrl: String? = null

        override suspend fun streamRelativeUrlTo(
            connectionInput: String,
            relativeUrl: String,
            output: OutputStream,
            serverCaPem: String?,
            clientIdentityJson: String?,
        ) {
            relativeUrlCallCount += 1
            lastRelativeUrl = relativeUrl
            relativeUrlFailure?.let { throw it }
            output.write(thumbnailBytes)
        }
    }

    private companion object {
        fun sampleThumbnail(): StoreIndexThumbnail {
            return StoreIndexThumbnail(
                url = "/media/thumbnail?key=gallery%2Fcat.png",
                profile = "grid",
                width = 256,
                height = 192,
                format = "jpeg",
                size_bytes = 1024,
            )
        }
    }
}
