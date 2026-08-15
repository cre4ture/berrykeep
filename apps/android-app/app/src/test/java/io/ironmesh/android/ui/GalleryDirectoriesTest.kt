package io.ironmesh.android.ui

import io.ironmesh.android.api.StoreIndexEntry
import org.junit.Assert.assertEquals
import org.junit.Test

class GalleryDirectoriesTest {
    @Test
    fun currentDirectoryOmitsAncestorAndCurrentFolderMarkers() {
        val entries = listOf(
            prefix("devices/"),
            prefix("devices/Oppo-uli/"),
            prefix("devices/Oppo-uli/Fotos/"),
            prefix("devices/Oppo-uli/Fotos/2026/"),
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
            galleryDirectoryItemsForParent(entries, "devices/Oppo-uli/"),
        )
    }

    @Test
    fun rootDirectoryIncludesOnlyTopLevelFolders() {
        val entries = listOf(
            prefix("devices/"),
            prefix("devices/Oppo-uli/"),
            prefix("archive/"),
        )

        assertEquals(
            listOf("devices", "archive"),
            galleryDirectoryItemsForParent(entries, "/").map(GalleryDirectoryItem::displayName),
        )
    }

    private fun prefix(path: String): StoreIndexEntry =
        StoreIndexEntry(path = path, entry_type = "prefix")

    private fun key(path: String): StoreIndexEntry =
        StoreIndexEntry(path = path, entry_type = "key")
}
