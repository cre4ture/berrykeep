package io.ironmesh.android.ui

import io.ironmesh.android.data.DeviceAuthState
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class DeviceIdentityUiStateTest {
    @Test
    fun composeProjectionContainsOnlyRedactedIdentityMetadata() {
        val auth = DeviceAuthState(
            clusterId = "cluster-visible",
            deviceId = "device-visible",
            label = "Phone",
            connectionInput = "bootstrap-secret",
            serverCaPem = "server-ca-secret",
            publicKeyPem = "public-key",
            privateKeyPem = "private-key-secret",
            credentialPem = "credential-secret",
            rendezvousClientIdentityPem = "rendezvous-secret",
            clientIdentityJson = "client-identity-json-secret",
        )

        val projection = auth.toDeviceIdentityUiState()
        val composeSnapshot = MainUiState(deviceIdentity = projection).toString()

        assertEquals("cluster-visible", projection.clusterId)
        assertEquals("device-visible", projection.deviceId)
        assertEquals("Phone", projection.label)
        assertTrue(projection.isEnrolled)
        listOf(
            "bootstrap-secret",
            "server-ca-secret",
            "public-key",
            "private-key-secret",
            "credential-secret",
            "rendezvous-secret",
            "client-identity-json-secret",
        ).forEach { secret ->
            assertFalse("Compose snapshot leaked $secret", composeSnapshot.contains(secret))
        }
        assertFalse(
            MainUiState::class.java.declaredFields.any { field ->
                DeviceAuthState::class.java.isAssignableFrom(field.type)
            },
        )
        assertEquals(
            setOf("clusterId", "deviceId", "label", "isEnrolled"),
            DeviceIdentityUiState::class.java.declaredFields
                .filterNot { field -> field.isSynthetic || field.name.startsWith("$") }
                .map { field -> field.name }
                .toSet(),
        )
    }

    @Test
    fun creatingUiIdentityDoesNotRedactCredentialsUsedByEnrollmentFlows() {
        val enrolledAuth = DeviceAuthState(
            clusterId = "cluster",
            deviceId = "device",
            connectionInput = "bootstrap",
            privateKeyPem = "private-key",
            publicKeyPem = "public-key",
            credentialPem = "credential",
            clientIdentityJson = "client-identity-json",
        )
        val projection = enrolledAuth.toDeviceIdentityUiState()

        assertTrue(projection.isEnrolled)
        assertEquals("private-key", enrolledAuth.privateKeyPem)
        assertEquals("credential", enrolledAuth.credentialPem)
        assertEquals("client-identity-json", enrolledAuth.clientIdentityJson)
        assertFalse(
            DeviceAuthState(clusterId = "cluster", deviceId = "device")
                .toDeviceIdentityUiState()
                .isEnrolled,
        )
    }
}
