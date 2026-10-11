package io.berrykeep.android.ui

import io.berrykeep.android.api.StoreIndexEntry
import org.junit.Assert.assertEquals
import org.junit.Test

class RemotePrefixPickerTest {
    @Test
    fun normalizesSelectedRemotePaths() {
        assertEquals(
            "photos/2026/trips",
            normalizedRemotePrefixPath(" /photos\\2026//trips/ "),
        )
        assertEquals("photos/2026", remotePrefixPickerParent("photos/2026/trips"))
        assertEquals("", remotePrefixPickerParent("photos"))
        assertEquals("/", remotePrefixPickerDisplayPath(""))
        assertEquals("/photos/2026", remotePrefixPickerDisplayPath("/photos/2026/"))
    }

    @Test
    fun listsOnlyRemoteFoldersInStableOrder() {
        val folders = remotePrefixFolders(
            listOf(
                StoreIndexEntry(path = "zebra/", entry_type = "prefix"),
                StoreIndexEntry(path = "Photos/", entry_type = "prefix"),
                StoreIndexEntry(path = "docs/readme.md", entry_type = "key"),
                StoreIndexEntry(path = "photos/", entry_type = "prefix"),
                StoreIndexEntry(path = "archive/", entry_type = "key"),
            ),
        )

        assertEquals(
            listOf(
                RemotePrefixFolder(path = "archive", displayName = "archive"),
                RemotePrefixFolder(path = "Photos", displayName = "Photos"),
                RemotePrefixFolder(path = "photos", displayName = "photos"),
                RemotePrefixFolder(path = "zebra", displayName = "zebra"),
            ),
            folders,
        )
    }
}
