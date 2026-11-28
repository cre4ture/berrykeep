package io.berrykeep.android.data

/** Keep the same sync identity and local root when renewing a folder grant. */
fun FolderSyncConfig.withRestoredFolderAccess(localFolder: String, treeUri: String): FolderSyncConfig {
    require(localFolder.trimEnd('/') == this.localFolder.trimEnd('/')) {
        "Select the original folder: ${this.localFolder}"
    }
    require(treeUri.isNotBlank()) { "Folder access requires a document tree URI" }
    return copy(localFolderTreeUri = treeUri)
}
