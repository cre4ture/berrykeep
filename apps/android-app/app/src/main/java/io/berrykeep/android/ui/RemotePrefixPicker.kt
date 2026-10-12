package io.berrykeep.android.ui

import androidx.compose.runtime.Immutable
import io.berrykeep.android.api.StoreIndexEntry

@Immutable
data class RemotePrefixPickerState(
    val currentPath: String = "",
    val folders: List<RemotePrefixFolder> = emptyList(),
    val loading: Boolean = false,
    val error: String? = null,
)

@Immutable
data class RemotePrefixFolder(
    val path: String,
    val displayName: String,
)

internal fun normalizedRemotePrefixPath(path: String): String =
    path
        .trim()
        .replace('\\', '/')
        .split('/')
        .filter(String::isNotBlank)
        .joinToString("/")

internal fun remotePrefixPickerParent(path: String): String =
    normalizedRemotePrefixPath(path).substringBeforeLast('/', missingDelimiterValue = "")

internal fun remotePrefixPickerDisplayPath(path: String): String =
    normalizedRemotePrefixPath(path).let { normalized ->
        if (normalized.isEmpty()) "/" else "/$normalized"
    }

internal fun remotePrefixFolders(entries: List<StoreIndexEntry>): List<RemotePrefixFolder> =
    entries
        .asSequence()
        .filter { entry -> entry.entry_type == "prefix" || entry.path.endsWith('/') }
        .map { entry -> normalizedRemotePrefixPath(entry.path) }
        .filter(String::isNotBlank)
        .distinct()
        .sortedWith(String.CASE_INSENSITIVE_ORDER)
        .map { path ->
            RemotePrefixFolder(
                path = path,
                displayName = path.substringAfterLast('/'),
            )
        }
        .toList()
