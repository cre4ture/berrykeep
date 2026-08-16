package io.ironmesh.android.data

import com.squareup.moshi.Json
import org.json.JSONObject

const val MIN_NODE_CONNECTION_PRIORITY = -20
const val MAX_NODE_CONNECTION_PRIORITY = 20

data class DeviceAuthState(
    val clusterId: String = "",
    val deviceId: String = "",
    val label: String? = null,
    @Json(name = "connectionInput")
    val connectionInput: String = "",
    val serverCaPem: String? = null,
    val publicKeyPem: String? = null,
    val privateKeyPem: String? = null,
    val credentialPem: String? = null,
    val rendezvousClientIdentityPem: String? = null,
    @Json(name = "clientIdentityJson")
    val clientIdentityJson: String? = null,
) {
    fun hasClientIdentity(): Boolean =
        !clientIdentityJson.isNullOrBlank() ||
            (clusterId.isNotBlank() &&
                deviceId.isNotBlank() &&
                !publicKeyPem.isNullOrBlank() &&
                !privateKeyPem.isNullOrBlank() &&
                !credentialPem.isNullOrBlank())

    fun connectionBootstrapJson(): String = connectionInput.trim()

    fun nodePriorityOverrides(): Map<String, Int> {
        val bootstrap = runCatching { JSONObject(connectionBootstrapJson()) }.getOrNull()
            ?: return emptyMap()
        val overrides = bootstrap.optJSONObject("node_priority_overrides") ?: return emptyMap()
        return overrides.keys().asSequence().mapNotNull { nodeId ->
            val priority = overrides.optInt(nodeId, Int.MIN_VALUE)
            if (priority in MIN_NODE_CONNECTION_PRIORITY..MAX_NODE_CONNECTION_PRIORITY) {
                nodeId to priority
            } else {
                null
            }
        }.toMap()
    }

    fun withNodePriorityOverride(nodeId: String, priority: Int?): DeviceAuthState {
        val normalizedNodeId = nodeId.trim()
        require(normalizedNodeId.isNotEmpty()) { "Server node ID is required" }
        if (priority != null) {
            require(priority in MIN_NODE_CONNECTION_PRIORITY..MAX_NODE_CONNECTION_PRIORITY) {
                "Server node priority must be between $MIN_NODE_CONNECTION_PRIORITY and $MAX_NODE_CONNECTION_PRIORITY"
            }
        }
        val bootstrap = JSONObject(connectionBootstrapJson())
        val overrides = bootstrap.optJSONObject("node_priority_overrides") ?: JSONObject()
        if (priority == null) {
            overrides.remove(normalizedNodeId)
        } else {
            overrides.put(normalizedNodeId, priority)
        }
        if (overrides.length() == 0) {
            bootstrap.remove("node_priority_overrides")
        } else {
            bootstrap.put("node_priority_overrides", overrides)
        }
        return copy(connectionInput = bootstrap.toString())
    }

    fun toClientIdentityJson(): String? {
        clientIdentityJson?.trim()?.takeIf { it.isNotEmpty() }?.let { return it }

        if (!hasClientIdentity()) {
            return null
        }

        return JSONObject().apply {
            put("cluster_id", clusterId)
            put("device_id", deviceId)
            if (!label.isNullOrBlank()) {
                put("label", label)
            }
            put("private_key_pem", privateKeyPem)
            put("public_key_pem", publicKeyPem)
            put("credential_pem", credentialPem)
            if (!rendezvousClientIdentityPem.isNullOrBlank()) {
                put("rendezvous_client_identity_pem", rendezvousClientIdentityPem)
            }
        }.toString()
    }
}
