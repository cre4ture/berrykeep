package io.ironmesh.android.ui

internal fun MainUiState.withGalleryRefreshStarted(): MainUiState =
    copy(
        galleryLoading = true,
        galleryError = null,
        galleryCollection = null,
        galleryPages = emptyMap(),
        status = "Loading gallery...",
    )

internal fun MainUiState.withGalleryRefreshFailure(error: Throwable): MainUiState {
    val galleryError = galleryLoadErrorFrom(error)
    return copy(
        galleryLoading = false,
        galleryError = galleryError,
        status = galleryLoadFailureStatus(galleryError),
    )
}

internal fun galleryLoadErrorFrom(error: Throwable): GalleryLoadError {
    val causes = generateSequence(error) { current -> current.cause }
        .take(8)
        .toList()
    val technicalDetail = causes
        .mapNotNull { cause -> cause.message?.trim()?.takeIf(String::isNotEmpty) }
        .lastOrNull()
        ?: causes.lastOrNull()?.javaClass?.simpleName?.takeIf(String::isNotEmpty)
    val timedOut = causes.any { cause ->
        cause.javaClass.simpleName.contains("timeout", ignoreCase = true) ||
            cause.message?.let { message ->
                message.contains("timeout", ignoreCase = true) ||
                    message.contains("timed out", ignoreCase = true)
            } == true
    }
    return GalleryLoadError(
        kind = if (timedOut) GalleryLoadErrorKind.TIMEOUT else GalleryLoadErrorKind.REQUEST_FAILED,
        technicalDetail = technicalDetail,
    )
}

private fun galleryLoadFailureStatus(error: GalleryLoadError): String {
    val summary = when (error.kind) {
        GalleryLoadErrorKind.TIMEOUT -> "Gallery index request timed out"
        GalleryLoadErrorKind.REQUEST_FAILED -> "Gallery index request failed"
    }
    return error.technicalDetail
        ?.takeIf { it.isNotBlank() }
        ?.let { detail -> "$summary: $detail" }
        ?: summary
}
