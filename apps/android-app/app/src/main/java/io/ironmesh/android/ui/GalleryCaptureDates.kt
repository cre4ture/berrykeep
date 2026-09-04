package io.ironmesh.android.ui

import java.time.LocalDate
import java.time.ZoneId

internal data class GalleryCaptureTimestampRange(
    val fromUnix: Long?,
    val untilUnix: Long?,
)

internal fun GalleryCaptureDateRange.toTimestampRange(
    zoneId: ZoneId = ZoneId.systemDefault(),
): GalleryCaptureTimestampRange {
    val start = startEpochDay ?: return GalleryCaptureTimestampRange(null, null)
    val end = requireNotNull(endEpochDay)
    return GalleryCaptureTimestampRange(
        fromUnix = LocalDate.ofEpochDay(start).atStartOfDay(zoneId).toEpochSecond(),
        untilUnix = LocalDate.ofEpochDay(end).plusDays(1).atStartOfDay(zoneId).toEpochSecond(),
    )
}
