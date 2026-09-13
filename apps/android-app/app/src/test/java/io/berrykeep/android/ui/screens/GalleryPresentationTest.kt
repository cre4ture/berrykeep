package io.berrykeep.android.ui.screens

import io.berrykeep.android.ui.GalleryCollectionState
import io.berrykeep.android.ui.GalleryDirectoryItem
import io.berrykeep.android.ui.GalleryLoadError
import io.berrykeep.android.ui.GalleryLoadErrorKind
import io.berrykeep.android.ui.GalleryPageState
import io.berrykeep.android.ui.GalleryPageStatus
import io.berrykeep.android.ui.MainUiState
import io.berrykeep.android.ui.galleryLoadErrorFrom
import io.berrykeep.android.ui.toLibraryUiState
import io.berrykeep.android.ui.withGalleryRefreshFailure
import io.berrykeep.android.ui.withGalleryRefreshStarted
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class GalleryPresentationTest {
    @Test
    fun startingRefreshClearsPersistentErrorAndStaleContent() {
        val state = MainUiState(
            galleryCollection = GalleryCollectionState(
                totalItemCount = 1,
                pageSize = 32,
                pageCount = 1,
            ),
            galleryPages = mapOf(
                0 to GalleryPageState(status = GalleryPageStatus.READY),
            ),
            galleryError = GalleryLoadError(
                kind = GalleryLoadErrorKind.REQUEST_FAILED,
                technicalDetail = "previous failure",
            ),
        )

        val refreshing = state.withGalleryRefreshStarted()

        assertTrue(refreshing.galleryLoading)
        assertNull(refreshing.galleryError)
        assertNull(refreshing.galleryCollection)
        assertTrue(refreshing.galleryPages.isEmpty())
        assertEquals(
            GalleryContentPresentation.LOADING,
            galleryContentPresentation(refreshing.toLibraryUiState()),
        )
    }

    @Test
    fun failedRefreshPersistsErrorInsteadOfPresentingEmptyLibrary() {
        val failed = MainUiState(galleryLoading = true)
            .withGalleryRefreshFailure(IllegalStateException("server unavailable"))

        assertEquals(
            GalleryLoadErrorKind.REQUEST_FAILED,
            failed.galleryError?.kind,
        )
        assertEquals("server unavailable", failed.galleryError?.technicalDetail)
        assertEquals(
            GalleryContentPresentation.LOAD_ERROR,
            galleryContentPresentation(failed.toLibraryUiState()),
        )
    }

    @Test
    fun nestedTimeoutGetsAnUnderstandableTimeoutClassification() {
        val error = galleryLoadErrorFrom(
            IllegalStateException(
                "gallery request failed",
                java.util.concurrent.TimeoutException("response timed out after 10s"),
            ),
        )

        assertEquals(GalleryLoadErrorKind.TIMEOUT, error.kind)
        assertEquals("response timed out after 10s", error.technicalDetail)
    }

    @Test
    fun directoriesWithoutImagesUseTheDirectoryEmptyPresentation() {
        val state = MainUiState(
            galleryCollection = GalleryCollectionState(
                totalItemCount = 0,
                pageSize = 32,
                pageCount = 0,
            ),
            galleryDirectories = listOf(
                GalleryDirectoryItem(
                    documentId = "dir:photos",
                    displayName = "photos",
                    pathLabel = "photos/",
                ),
            ),
        )

        assertEquals(
            GalleryContentPresentation.EMPTY_CURRENT_DIRECTORY,
            galleryContentPresentation(state.toLibraryUiState()),
        )
    }

    @Test
    fun fullResolutionIsRequestedOnlyAfterZoomingPastFitScale() {
        assertFalse(shouldRequestGalleryFullResolution(1f))
        assertFalse(shouldRequestGalleryFullResolution(1.01f))
        assertTrue(shouldRequestGalleryFullResolution(1.02f))
    }

    @Test
    fun galleryImageResolutionProgressivelyIncreasesWithZoom() {
        assertNull(galleryImageResolutionForScale(1f))
        assertEquals(GalleryImageResolution.DETAIL, galleryImageResolutionForScale(1.02f))
        assertEquals(GalleryImageResolution.DETAIL, galleryImageResolutionForScale(2f))
        assertEquals(GalleryImageResolution.HIGH, galleryImageResolutionForScale(2.01f))
        assertEquals(GalleryImageResolution.HIGH, galleryImageResolutionForScale(3.5f))
        assertEquals(GalleryImageResolution.ORIGINAL, galleryImageResolutionForScale(3.51f))
    }

    @Test
    fun galleryImageResolutionOnlyLoadsWhenItImprovesTheDisplayedImage() {
        assertFalse(shouldLoadGalleryImageResolution(null, null))
        assertTrue(shouldLoadGalleryImageResolution(GalleryImageResolution.DETAIL, null))
        assertFalse(
            shouldLoadGalleryImageResolution(
                GalleryImageResolution.DETAIL,
                GalleryImageResolution.DETAIL,
            ),
        )
        assertTrue(
            shouldLoadGalleryImageResolution(
                GalleryImageResolution.HIGH,
                GalleryImageResolution.DETAIL,
            ),
        )
        assertFalse(
            shouldLoadGalleryImageResolution(
                GalleryImageResolution.DETAIL,
                GalleryImageResolution.HIGH,
            ),
        )
    }
}
