package io.ironmesh.android.ui

import kotlinx.coroutines.CancellationException

internal suspend fun runAppConnectionStatusFlush(
    flush: suspend () -> Unit,
    onFailure: (Throwable) -> Unit,
) {
    try {
        flush()
    } catch (error: CancellationException) {
        throw error
    } catch (error: Throwable) {
        onFailure(error)
    }
}
