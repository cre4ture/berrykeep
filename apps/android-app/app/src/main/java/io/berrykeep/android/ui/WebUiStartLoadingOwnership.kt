package io.berrykeep.android.ui

/** Tracks which Web UI start is allowed to complete the shared loading indicator. */
internal class WebUiStartLoadingOwnership {
    private var ownerGeneration: Long? = null

    fun begin(generation: Long) {
        ownerGeneration = generation
    }

    /** Returns true only when this operation still owns the loading state. */
    fun releaseIfOwner(generation: Long): Boolean {
        if (ownerGeneration != generation) {
            return false
        }
        ownerGeneration = null
        return true
    }

    fun hasOwner(): Boolean = ownerGeneration != null
}
