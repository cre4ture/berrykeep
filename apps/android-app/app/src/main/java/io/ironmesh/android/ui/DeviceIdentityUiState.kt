package io.ironmesh.android.ui

import androidx.compose.runtime.Immutable
import io.ironmesh.android.data.DeviceAuthState

/**
 * Deliberately redacted device identity suitable for snapshots owned by Compose.
 *
 * Connection bootstraps, certificates, credentials, and private keys must remain in the data
 * layer or behind the private boundary of [MainViewModel].
 */
@Immutable
data class DeviceIdentityUiState(
    val clusterId: String = "",
    val deviceId: String = "",
    val label: String? = null,
    val isEnrolled: Boolean = false,
)

internal fun DeviceAuthState.toDeviceIdentityUiState(): DeviceIdentityUiState =
    DeviceIdentityUiState(
        clusterId = clusterId,
        deviceId = deviceId,
        label = label,
        isEnrolled = hasClientIdentity(),
    )
