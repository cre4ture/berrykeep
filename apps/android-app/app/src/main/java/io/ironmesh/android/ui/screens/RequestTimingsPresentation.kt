package io.ironmesh.android.ui.screens

import io.ironmesh.android.data.ConnectionRouteAttemptSnapshot
import io.ironmesh.android.data.ConnectionRouteEndpointSnapshot
import io.ironmesh.android.data.ConnectionRouteSnapshot

internal data class TimedRequest(
    val endpoint: ConnectionRouteEndpointSnapshot,
    val attempt: ConnectionRouteAttemptSnapshot,
)

internal data class SessionPoolTotals(
    val connectCount: Long = 0L,
    val reuseCount: Long = 0L,
    val resetCount: Long = 0L,
    val connectDurationUs: Long = 0L,
    val relayPairingDurationUs: Long = 0L,
)

internal data class TimingMeasurementPresentation(
    val requests: List<TimedRequest>,
    val successfulRequestCount: Int,
    val averageTotalUs: Double?,
    val averageServerUs: Double?,
    val averageTransportUs: Double?,
    val sessionPool: SessionPoolTotals,
    val averageSessionSetupUs: Double?,
    val averageRelayPairingUs: Double?,
    val bestClockSample: ConnectionRouteAttemptSnapshot?,
)

internal fun timingMeasurementPresentation(
    snapshot: ConnectionRouteSnapshot?,
): TimingMeasurementPresentation {
    val requests = snapshot
        ?.endpoints
        .orEmpty()
        .flatMap { endpoint -> endpoint.recentAttempts.map { attempt -> TimedRequest(endpoint, attempt) } }
        .sortedByDescending { request -> request.attempt.finishedUnixMs ?: request.attempt.startedUnixMs }
    val successful = requests.filter { request ->
        request.attempt.outcome == "success" && request.attempt.totalDurationUs != null
    }
    val sessionPool = snapshot?.endpoints.orEmpty().fold(SessionPoolTotals()) { total, endpoint ->
        val stats = endpoint.transportSessionPool
        total.copy(
            connectCount = total.connectCount + stats.connectCount,
            reuseCount = total.reuseCount + stats.reuseCount,
            resetCount = total.resetCount + stats.resetCount,
            connectDurationUs = total.connectDurationUs + stats.connectDurationUs,
            relayPairingDurationUs = total.relayPairingDurationUs + stats.relayPairingDurationUs,
        )
    }
    val relayConnectCount = snapshot?.endpoints.orEmpty()
        .filter { endpoint -> endpoint.pathKind == "relay_tunnel" }
        .sumOf { endpoint -> endpoint.transportSessionPool.connectCount }

    return TimingMeasurementPresentation(
        requests = requests,
        successfulRequestCount = successful.size,
        averageTotalUs = successful.mapNotNull { it.attempt.totalDurationUs }.averageOrNull(),
        averageServerUs = successful.mapNotNull { it.attempt.serverProcessingDurationUs }.averageOrNull(),
        averageTransportUs = successful.mapNotNull { it.attempt.transportOverheadUs }.averageOrNull(),
        sessionPool = sessionPool,
        averageSessionSetupUs = sessionPool.connectDurationUs.takeIf { sessionPool.connectCount > 0L }
            ?.toDouble()
            ?.div(sessionPool.connectCount),
        averageRelayPairingUs = sessionPool.relayPairingDurationUs.takeIf { relayConnectCount > 0L }
            ?.toDouble()
            ?.div(relayConnectCount),
        bestClockSample = successful
            .map { request -> request.attempt }
            .filter { attempt -> attempt.clockOffsetUs != null && attempt.clockUncertaintyUs != null }
            .minByOrNull { attempt -> attempt.clockUncertaintyUs ?: Long.MAX_VALUE },
    )
}

private fun List<Long>.averageOrNull(): Double? {
    return if (isEmpty()) null else average()
}
