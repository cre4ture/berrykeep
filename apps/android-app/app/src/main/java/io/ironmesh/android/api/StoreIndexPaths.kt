package io.ironmesh.android.api

internal fun isDirectStoreIndexChildPath(
    parentPath: String,
    candidatePath: String,
): Boolean {
    val normalizedParent = parentPath.trim().trim('/')
    val normalizedCandidate = candidatePath.trim().trim('/')
    if (normalizedCandidate.isBlank()) {
        return false
    }
    if (normalizedParent.isBlank()) {
        return !normalizedCandidate.contains('/')
    }
    if (normalizedCandidate == normalizedParent) {
        return false
    }
    val prefix = "$normalizedParent/"
    if (!normalizedCandidate.startsWith(prefix)) {
        return false
    }
    val remainder = normalizedCandidate.removePrefix(prefix)
    return remainder.isNotBlank() && !remainder.contains('/')
}
