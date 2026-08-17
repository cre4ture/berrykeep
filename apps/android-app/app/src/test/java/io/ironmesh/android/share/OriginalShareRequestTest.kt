package io.ironmesh.android.share

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Test

class OriginalShareRequestTest {
    @Test
    fun fromWebMessage_requiresAndPreservesImmutableVersionSelector() {
        val request = OriginalShareRequest.fromWebMessage(
            """
            {
              "action":"share-original",
              "requestId":"request-1",
              "key":"gallery/cat.png",
              "snapshotId":null,
              "versionId":"version-cat-001",
              "fileName":"../cat.png",
              "mimeType":"Image/PNG",
              "sizeBytes":3145728
            }
            """.trimIndent(),
        )

        assertEquals("request-1", request.requestId)
        assertEquals("gallery/cat.png", request.remotePath)
        assertNull(request.snapshotId)
        assertEquals("version-cat-001", request.versionId)
        assertEquals("cat.png", request.displayName)
        assertEquals("image/png", request.mimeType)
        assertEquals(3_145_728L, request.sizeBytes)
    }

    @Test
    fun fromWebMessage_rejectsMutableOrAmbiguousSelectors() {
        assertThrows(IllegalArgumentException::class.java) {
            OriginalShareRequest.fromWebMessage(
                validMessage.replace("\"versionId\":\"version-1\"", "\"versionId\":null"),
            )
        }
        assertThrows(IllegalArgumentException::class.java) {
            OriginalShareRequest.fromWebMessage(
                validMessage.replace("\"snapshotId\":null", "\"snapshotId\":\"snapshot-1\""),
            )
        }
    }

    @Test
    fun fromWebMessage_rejectsNegativeSizes() {
        assertThrows(IllegalArgumentException::class.java) {
            OriginalShareRequest.fromWebMessage(validMessage.replace("\"sizeBytes\":12", "\"sizeBytes\":-1"))
        }
    }

    private companion object {
        val validMessage =
            """{"action":"share-original","requestId":"request-1","key":"file.bin","snapshotId":null,"versionId":"version-1","fileName":"file.bin","mimeType":"application/octet-stream","sizeBytes":12}"""
    }
}
