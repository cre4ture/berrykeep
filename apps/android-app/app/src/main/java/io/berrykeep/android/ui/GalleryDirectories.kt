package io.berrykeep.android.ui

import io.berrykeep.android.api.StoreIndexEntry

internal const val GALLERY_ROOT_DOCUMENT_ID = "dir:"
internal const val GALLERY_ROOT_PATH = "/"

internal fun galleryDirectoryItems(entries: List<StoreIndexEntry>): List<GalleryDirectoryItem> =
    entries
        .mapNotNull(::galleryDirectoryItem)
        .distinctBy(GalleryDirectoryItem::documentId)

private fun galleryDirectoryItem(entry: StoreIndexEntry): GalleryDirectoryItem? {
    if (entry.entry_type != "prefix") {
        return null
    }
    val normalizedPath = entry.path.trim().trim('/')
    return GalleryDirectoryItem(
        documentId = "dir:$normalizedPath",
        displayName = normalizedPath.substringAfterLast('/'),
        pathLabel = "$normalizedPath/",
    )
}
