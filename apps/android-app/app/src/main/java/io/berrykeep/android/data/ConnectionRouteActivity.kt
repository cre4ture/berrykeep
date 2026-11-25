package io.berrykeep.android.data

private const val ROUTE_ACTIVE_WINDOW_MS = 2_000L

internal fun ConnectionRouteEndpointSnapshot.isHealthyAt(observedAtUnixMs: Long?): Boolean {
    return totalSuccesses > 0L && consecutiveFailures == 0 &&
        (circuitOpenUntilUnixMs == null || observedAtUnixMs != null && circuitOpenUntilUnixMs <= observedAtUnixMs)
}

/** Real request completion confirms activity; attempts and health probes cannot. */
internal fun ConnectionRouteEndpointSnapshot.hasRecentSuccessfulActivity(observedAtUnixMs: Long?): Boolean {
    val observedAt = observedAtUnixMs ?: return false
    val succeededAt = lastSuccessfulUseUnixMs ?: return false
    return isHealthyAt(observedAt) &&
        (lastFailureUnixMs == null || succeededAt >= lastFailureUnixMs) &&
        succeededAt <= observedAt &&
        observedAt - succeededAt <= ROUTE_ACTIVE_WINDOW_MS
}
