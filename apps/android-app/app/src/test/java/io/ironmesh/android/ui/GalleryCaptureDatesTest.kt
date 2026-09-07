package io.ironmesh.android.ui

import java.time.LocalDate
import java.time.ZoneId
import org.junit.Assert.assertEquals
import org.junit.Test

class GalleryCaptureDatesTest {
    @Test
    fun dateRangeUsesInclusiveLocalCalendarDaysAndExclusiveUpperBound() {
        val zone = ZoneId.of("Europe/Berlin")
        val start = LocalDate.of(2026, 3, 28)
        val end = LocalDate.of(2026, 3, 29)

        val timestamps = GalleryCaptureDateRange(
            startEpochDay = start.toEpochDay(),
            endEpochDay = end.toEpochDay(),
        ).toTimestampRange(zone)

        assertEquals(start.atStartOfDay(zone).toEpochSecond(), timestamps.fromUnix)
        assertEquals(end.plusDays(1).atStartOfDay(zone).toEpochSecond(), timestamps.untilUnix)
    }

    @Test
    fun emptyDateRangeDoesNotAddTimestampBounds() {
        assertEquals(
            GalleryCaptureTimestampRange(null, null),
            GalleryCaptureDateRange().toTimestampRange(ZoneId.of("UTC")),
        )
    }

    @Test
    fun preEpochDateRangeBecomesAnEmptySupportedTimestampRange() {
        val date = LocalDate.of(1965, 4, 1)

        assertEquals(
            GalleryCaptureTimestampRange(0, 0),
            GalleryCaptureDateRange(
                startEpochDay = date.toEpochDay(),
                endEpochDay = date.toEpochDay(),
            ).toTimestampRange(ZoneId.of("UTC")),
        )
    }

    @Test
    fun rangeCrossingEpochClampsOnlyItsLowerBound() {
        val zone = ZoneId.of("UTC")
        val start = LocalDate.of(1969, 12, 25)
        val end = LocalDate.of(1970, 1, 2)

        val timestamps = GalleryCaptureDateRange(
            startEpochDay = start.toEpochDay(),
            endEpochDay = end.toEpochDay(),
        ).toTimestampRange(zone)

        assertEquals(0L, timestamps.fromUnix)
        assertEquals(end.plusDays(1).atStartOfDay(zone).toEpochSecond(), timestamps.untilUnix)
    }
}
