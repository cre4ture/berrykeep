package io.berrykeep.android.data

private const val ROUTE_ACTIVE_WINDOW_MS = 2_000L

internal fun ConnectionRouteEndpointSnapshot.isHealthyAt(observedAtUnixMs: Long?): Boolean {
    return totalSuccesses > 0L && consecutiveFailures == 0 &&
        (circuitOpenUntilUnixMs == null || observedAtUnixMs != null && circuitOpenUntilUnixMs <= observedAtUnixMs)
}

/** A route attempt is recorded before I/O; only a completed success confirms activity. */
internal fun ConnectionRouteEndpointSnapshot.hasRecentSuccessfulActivity(observedAtUnixMs: Long?): Boolean {
    val observedAt = observedAtUnixMs ?: return false
    val usedAt = lastUsedUnixMs ?: return false
    val succeededAt = lastSuccessUnixMs ?: return false
    return isHealthyAt(observedAt) &&
        succeededAt >= usedAt &&
        (lastFailureUnixMs == null || succeededAt >= lastFailureUnixMs) &&
        succeededAt <= observedAt &&
        observedAt - succeededAt <= ROUTE_ACTIVE_WINDOW_MS
}
