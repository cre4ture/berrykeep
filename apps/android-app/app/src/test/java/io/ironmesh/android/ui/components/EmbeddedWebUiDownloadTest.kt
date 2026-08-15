package io.ironmesh.android.ui.components

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class EmbeddedWebUiDownloadTest {
    @Test
    fun sameOriginAllowsOnlyTheEmbeddedLoopbackListener() {
        val origin = "http://127.0.0.1:41873/?embedded_client=android"

        assertTrue(
            isSameEmbeddedWebUiOrigin(
                origin,
                "http://127.0.0.1:41873/api/v1/store/stream-binary?key=gallery%2Fcat.png",
            ),
        )
        assertFalse(isSameEmbeddedWebUiOrigin(origin, "http://127.0.0.1:41874/file.png"))
        assertFalse(isSameEmbeddedWebUiOrigin(origin, "https://example.test/file.png"))
    }

    @Test
    fun downloadFileNamePrefersTheServerDisposition() {
        assertEquals(
            "cat-original.png",
            embeddedWebUiDownloadFileName(
                "http://127.0.0.1:41873/api/v1/store/stream-binary?key=gallery%2Fcat.png",
                "inline; filename=\"cat-original.png\"",
            ),
        )
    }

    @Test
    fun downloadFileNameFallsBackToTheDecodedObjectKey() {
        assertEquals(
            "summer_photo.heic",
            embeddedWebUiDownloadFileName(
                "http://127.0.0.1:41873/api/v1/store/stream-binary?key=photos%2Fsummer_photo.heic",
                "",
            ),
        )
    }

    @Test
    fun emptyDispositionFileNameFallsBackToTheDecodedObjectKey() {
        assertEquals(
            "summer_photo.heic",
            embeddedWebUiDownloadFileName(
                "http://127.0.0.1:41873/api/v1/store/stream-binary?key=photos%2Fsummer_photo.heic",
                "attachment; filename=\"\"",
            ),
        )
    }

    @Test
    fun downloadFileNameRemovesPathAndUnsafeCharacters() {
        assertEquals(
            "photo_name_.png",
            embeddedWebUiDownloadFileName(
                "http://127.0.0.1:41873/download",
                "attachment; filename=\"../photo:name?.png\"",
            ),
        )
    }
}
