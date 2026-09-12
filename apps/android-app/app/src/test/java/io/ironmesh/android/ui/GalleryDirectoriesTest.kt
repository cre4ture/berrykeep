package io.ironmesh.android.ui

import io.ironmesh.android.api.StoreIndexEntry
import org.junit.Assert.assertEquals
import org.junit.Test

class GalleryDirectoriesTest {
    @Test
    fun childrenProjectionProducesDirectoryItems() {
        val entries = listOf(
            prefix("devices/Oppo-uli/Fotos/"),
            key("devices/Oppo-uli/image.jpg"),
        )

        assertEquals(
            listOf(
                GalleryDirectoryItem(
                    documentId = "dir:devices/Oppo-uli/Fotos",
                    displayName = "Fotos",
                    pathLabel = "devices/Oppo-uli/Fotos/",
                ),
            ),
            galleryDirectoryItems(entries),
        )
    }

    @Test
    fun rootChildrenProjectionIncludesTopLevelFolders() {
        val entries = listOf(
            prefix("devices/"),
            prefix("archive/"),
        )

        assertEquals(
            listOf("devices", "archive"),
            galleryDirectoryItems(entries).map(GalleryDirectoryItem::displayName),
        )
    }

    private fun prefix(path: String): StoreIndexEntry =
        StoreIndexEntry(path = path, entry_type = "prefix")

    private fun key(path: String): StoreIndexEntry =
        StoreIndexEntry(path = path, entry_type = "key")
}
