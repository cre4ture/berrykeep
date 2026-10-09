package io.berrykeep.android.data

import com.squareup.moshi.Json

data class TaskQueueEntry(
    val id: String = "",
    val label: String = "",
    val pending: Long = 0,
    val active: Long = 0,
    val capacity: Long? = null,
    val state: String = "idle",
    val detail: String? = null,
)

data class ClusterTaskQueueNodeSnapshot(
    @Json(name = "node_id")
    val nodeId: String = "",
    val queues: List<TaskQueueEntry> = emptyList(),
)

data class UnavailableTaskQueueNode(
    @Json(name = "node_id")
    val nodeId: String = "",
    val error: String = "",
)

data class ClusterTaskQueueSnapshot(
    @Json(name = "generated_at_unix_ms")
    val generatedAtUnixMs: Long = 0,
    val nodes: List<ClusterTaskQueueNodeSnapshot> = emptyList(),
    @Json(name = "unavailable_nodes")
    val unavailableNodes: List<UnavailableTaskQueueNode> = emptyList(),
)
