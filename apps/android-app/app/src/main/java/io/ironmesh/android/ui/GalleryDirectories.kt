package io.ironmesh.android.ui

import io.ironmesh.android.api.StoreIndexEntry
import io.ironmesh.android.api.isDirectStoreIndexChildPath

internal const val GALLERY_ROOT_DOCUMENT_ID = "dir:"
internal const val GALLERY_ROOT_PATH = "/"

internal fun galleryDirectoryItemsForParent(
    entries: List<StoreIndexEntry>,
    parentPath: String,
): List<GalleryDirectoryItem> =
    entries
        .mapNotNull { entry -> galleryDirectoryItemForParent(entry, parentPath) }
        .distinctBy(GalleryDirectoryItem::documentId)

private fun galleryDirectoryItemForParent(
    entry: StoreIndexEntry,
    parentPath: String,
): GalleryDirectoryItem? {
    if (entry.entry_type != "prefix") {
        return null
    }
    val normalizedPath = entry.path.trim().trim('/')
    if (!isDirectStoreIndexChildPath(parentPath, normalizedPath)) {
        return null
    }
    return GalleryDirectoryItem(
        documentId = "dir:$normalizedPath",
        displayName = normalizedPath.substringAfterLast('/'),
        pathLabel = "$normalizedPath/",
    )
}
